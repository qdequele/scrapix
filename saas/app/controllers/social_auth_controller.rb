require "net/http"

# Social OAuth login (Google, GitHub) — port of
# bins/scrapix-api/src/auth/social.rs. Same flow: redirect to the provider's
# consent screen, exchange the code on callback, find-or-create the user
# (linking oauth_identities by provider id, then by email), issue the session
# JWT cookie, redirect to the console dashboard. Errors redirect to the
# console login page with ?error=.
class SocialAuthController < ApplicationController
  PROVIDERS = %w[google github].freeze

  def initiate
    provider = params[:provider]
    config = provider_config(provider)
    unless PROVIDERS.include?(provider)
      return error_redirect("Unknown provider")
    end
    unless config
      return error_redirect("#{provider.capitalize} login is not configured")
    end

    state_token = SecureRandom.alphanumeric(32)
    callback_uri = "#{api_base_url}/auth/social/#{provider}/callback"
    OauthStateStore.insert(state_token, provider, callback_uri)

    query = {
      client_id: config[:client_id],
      redirect_uri: callback_uri,
      state: state_token
    }
    auth_url =
      case provider
      when "google"
        "https://accounts.google.com/o/oauth2/v2/auth?" + URI.encode_www_form(
          query.merge(response_type: "code", scope: "openid email profile",
                      access_type: "online", prompt: "select_account")
        )
      when "github"
        "https://github.com/login/oauth/authorize?" + URI.encode_www_form(
          query.merge(scope: "user:email")
        )
      end

    redirect_to auth_url, status: :temporary_redirect, allow_other_host: true
  end

  def callback
    provider = params[:provider]
    return error_redirect("Login cancelled: #{params[:error]}") if params[:error].present?
    return error_redirect("Missing authorization code") if params[:code].blank?
    return error_redirect("Missing state parameter") if params[:state].blank?

    stored_provider, callback_uri = OauthStateStore.take(params[:state])
    return error_redirect("Invalid or expired state") unless stored_provider
    return error_redirect("Provider mismatch") if stored_provider != provider

    config = provider_config(provider)
    return error_redirect("Unknown provider") unless config

    access_token = exchange_code(provider, config, params[:code], callback_uri)
    return error_redirect("Failed to authenticate with provider") unless access_token

    info = fetch_user_info(provider, access_token)
    return error_redirect("Failed to get profile from provider") unless info

    user_id, email = find_or_create_user(provider, info)
    return error_redirect("Failed to create account") unless user_id

    cookies["scrapix_session"] = SessionToken.cookie(SessionToken.encode(user_id, email))
    redirect_to "#{console_url}/dashboard", status: :see_other, allow_other_host: true
  end

  private

  def provider_config(provider)
    id = ENV["#{provider.to_s.upcase}_CLIENT_ID"]
    secret = ENV["#{provider.to_s.upcase}_CLIENT_SECRET"]
    return nil if id.blank? || secret.blank?

    { client_id: id, client_secret: secret }
  end

  def api_base_url
    ENV.fetch("SAAS_API_BASE_URL") { "http://localhost:#{ENV.fetch('SAAS_PORT', 8081)}" }
  end

  def console_url
    ENV.fetch("CONSOLE_URL", "http://localhost:3001")
  end

  def error_redirect(message)
    redirect_to "#{console_url}/login?error=#{CGI.escape(message)}",
                status: :temporary_redirect, allow_other_host: true
  end

  def exchange_code(provider, config, code, redirect_uri)
    url, headers =
      case provider
      when "google" then [ "https://oauth2.googleapis.com/token", {} ]
      when "github" then [ "https://github.com/login/oauth/access_token", { "Accept" => "application/json" } ]
      end
    form = {
      code: code, client_id: config[:client_id], client_secret: config[:client_secret],
      redirect_uri: redirect_uri
    }
    form[:grant_type] = "authorization_code" if provider == "google"

    body = http_post_form(url, form, headers)
    body && body["access_token"]
  rescue StandardError => e
    Rails.logger.warn("OAuth code exchange failed (#{provider}): #{e.message}")
    nil
  end

  def fetch_user_info(provider, access_token)
    case provider
    when "google"
      profile = http_get_json("https://www.googleapis.com/oauth2/v2/userinfo", access_token)
      return nil unless profile && profile["email"] && profile["id"]

      { email: profile["email"], name: profile["name"], provider_user_id: profile["id"].to_s }
    when "github"
      profile = http_get_json("https://api.github.com/user", access_token)
      return nil unless profile && profile["id"]

      email = profile["email"].presence || github_primary_email(access_token)
      return nil unless email

      { email: email, name: profile["name"], provider_user_id: profile["id"].to_s }
    end
  rescue StandardError => e
    Rails.logger.warn("OAuth profile fetch failed (#{provider}): #{e.message}")
    nil
  end

  def github_primary_email(access_token)
    emails = http_get_json("https://api.github.com/user/emails", access_token)
    return nil unless emails.is_a?(Array)

    emails.find { |e| e["primary"] && e["verified"] }&.fetch("email", nil)
  end

  def find_or_create_user(provider, info)
    if (identity = OauthIdentity.find_by(provider: provider, provider_user_id: info[:provider_user_id]))
      user = identity.user
      return [ user.id, user.email ]
    end

    if (user = User.find_by(email: info[:email]))
      OauthIdentity.create_or_find_by(provider: provider, provider_user_id: info[:provider_user_id]) do |i|
        i.user_id = user.id
      end
      user.update_columns(email_verified: true) unless user.email_verified
      return [ user.id, user.email ]
    end

    user = nil
    ActiveRecord::Base.transaction do
      user = User.create!(email: info[:email], full_name: info[:name], email_verified: true, password_hash: nil)
      account = Account.create!(name: "#{info[:name] || info[:email]}'s Account")
      AccountMember.create!(user_id: user.id, account_id: account.id, role: "owner")
      Transaction.create!(
        account_id: account.id, type: "initial_deposit", amount: 100,
        balance_after: 100, description: "Welcome credit deposit"
      )
      OauthIdentity.create!(user_id: user.id, provider: provider, provider_user_id: info[:provider_user_id])
    end

    auto_accept_invites(user)
    EmailQueue.welcome(user.email, user.full_name)
    [ user.id, user.email ]
  rescue ActiveRecord::ActiveRecordError => e
    Rails.logger.warn("Social find_or_create failed (#{provider}): #{e.message}")
    nil
  end

  def auto_accept_invites(user)
    AccountInvite.pending.where("expires_at > now()").where(email: user.email).find_each do |invite|
      AccountMember.create_or_find_by(user_id: user.id, account_id: invite.account_id) do |m|
        m.role = invite.role
      end
      invite.update_columns(status: "accepted")
    rescue ActiveRecord::ActiveRecordError => e
      Rails.logger.warn("Failed to auto-accept invite #{invite.id}: #{e.message}")
    end
  end

  def http_post_form(url, form, headers)
    uri = URI.parse(url)
    http = build_http(uri)
    request = Net::HTTP::Post.new(uri)
    headers.each { |k, v| request[k] = v }
    request.set_form_data(form)
    response = http.request(request)
    JSON.parse(response.body) if response.is_a?(Net::HTTPSuccess)
  end

  def http_get_json(url, bearer)
    uri = URI.parse(url)
    http = build_http(uri)
    request = Net::HTTP::Get.new(uri)
    request["Authorization"] = "Bearer #{bearer}"
    request["User-Agent"] = "scrapix"
    response = http.request(request)
    JSON.parse(response.body) if response.is_a?(Net::HTTPSuccess)
  end

  def build_http(uri)
    http = Net::HTTP.new(uri.host, uri.port)
    http.use_ssl = uri.scheme == "https"
    http.open_timeout = 5
    http.read_timeout = 15
    http
  end
end
