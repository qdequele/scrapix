# OAuth 2.1 provider — port of bins/scrapix-api/src/auth/oauth.rs.
#
# Implements RFC 8414 (server metadata), RFC 7591 (dynamic client
# registration), RFC 7636 (PKCE, S256 only), and RFC 7009 (revocation), plus
# the additive RFC 9728 protected-resource metadata that newer MCP clients
# use for discovery (the Rust API never served it).
#
# Flow: MCP client → discover metadata → register client → authorize (browser
# login form) → exchange code for tokens → call /mcp with the Bearer token.
#
# Expired code/token cleanup stays with the Rust engine's hourly task, like
# scheduled-email delivery.
class OauthController < ApplicationController
  ACCESS_TOKEN_TTL = 1.hour
  REFRESH_TOKEN_TTL = 30.days

  # GET /.well-known/oauth-authorization-server (RFC 8414)
  def metadata
    base = base_url
    render json: {
      issuer: base,
      authorization_endpoint: "#{base}/oauth/authorize",
      token_endpoint: "#{base}/oauth/token",
      registration_endpoint: "#{base}/oauth/register",
      revocation_endpoint: "#{base}/oauth/revoke",
      response_types_supported: [ "code" ],
      grant_types_supported: [ "authorization_code", "refresh_token" ],
      code_challenge_methods_supported: [ "S256" ],
      token_endpoint_auth_methods_supported: [ "none" ],
      scopes_supported: [ "mcp" ]
    }
  end

  # GET /.well-known/oauth-protected-resource (RFC 9728)
  def protected_resource
    base = base_url
    render json: {
      resource: "#{base}/mcp",
      authorization_servers: [ base ],
      scopes_supported: [ "mcp" ],
      bearer_methods_supported: [ "header" ]
    }
  end

  # POST /oauth/register (RFC 7591)
  def register
    redirect_uris = params[:redirect_uris]
    unless redirect_uris.is_a?(Array) && redirect_uris.any?
      return oauth_error(:bad_request, "invalid_client_metadata", "At least one redirect_uri is required")
    end

    redirect_uris = redirect_uris.map(&:to_s)
    redirect_uris.each do |uri|
      unless absolute_url?(uri)
        return oauth_error(:bad_request, "invalid_client_metadata", "Invalid redirect_uri: #{uri}")
      end
    end

    client_id = "sxc_#{SecureRandom.alphanumeric(32)}"
    client_name = params[:client_name].presence
    OauthClient.create!(client_id: client_id, client_name: client_name, redirect_uris: redirect_uris)

    render json: { client_id: client_id, client_name: client_name, redirect_uris: redirect_uris }
  end

  # GET /oauth/authorize — render the login form
  def authorize_form
    if params[:response_type] != "code"
      return oauth_error(:bad_request, "unsupported_response_type", "Only response_type=code is supported")
    end

    method = params[:code_challenge_method].presence || "S256"
    if method != "S256"
      return oauth_error(:bad_request, "invalid_request", "Only S256 code_challenge_method is supported")
    end

    client = OauthClient.find_by(client_id: params[:client_id].to_s)
    return oauth_error(:bad_request, "invalid_client", "Unknown client_id") if client.nil?

    unless client.redirect_uris.include?(params[:redirect_uri].to_s)
      return oauth_error(:bad_request, "invalid_request", "redirect_uri not registered for this client")
    end

    render html: login_form_html(
      client_id: params[:client_id].to_s,
      redirect_uri: params[:redirect_uri].to_s,
      code_challenge: params[:code_challenge].to_s,
      code_challenge_method: method,
      state: params[:state].to_s
    ).html_safe
  end

  # POST /oauth/authorize — validate credentials, issue code, redirect (307)
  def authorize
    user = User.find_by(email: params[:email].to_s)
    valid = user.present? &&
            (Argon2::Password.verify_password(params[:password].to_s, user.password_hash) rescue false)
    return authorize_error_page("Invalid email or password") unless valid

    unless OauthClient.exists?(client_id: params[:client_id].to_s)
      return authorize_error_page("Invalid client")
    end

    code = "sxac_#{SecureRandom.alphanumeric(48)}"
    OauthAuthorizationCode.create!(
      code: code,
      client_id: params[:client_id].to_s,
      user_id: user.id,
      redirect_uri: params[:redirect_uri].to_s,
      code_challenge: params[:code_challenge].to_s,
      code_challenge_method: params[:code_challenge_method].to_s,
      expires_at: 10.minutes.from_now
    )

    redirect_url = URI.parse(params[:redirect_uri].to_s)
    query = URI.decode_www_form(redirect_url.query.to_s)
    query << [ "code", code ]
    query << [ "state", params[:state].to_s ] if params[:state].present?
    redirect_url.query = URI.encode_www_form(query)

    redirect_to redirect_url.to_s, status: :temporary_redirect, allow_other_host: true
  rescue URI::InvalidURIError
    authorize_error_page("Invalid redirect URI")
  end

  # POST /oauth/token — authorization_code exchange or refresh_token rotation
  def token
    case params[:grant_type]
    when "authorization_code" then exchange_authorization_code
    when "refresh_token" then refresh_tokens
    else
      oauth_error(:bad_request, "unsupported_grant_type", "Supported: authorization_code, refresh_token")
    end
  end

  # POST /oauth/revoke (RFC 7009 — always 200, even for unknown tokens)
  def revoke
    token_hash = Digest::SHA256.hexdigest(params[:token].to_s)
    OauthToken.where(token_hash: token_hash, revoked: false).update_all(revoked: true)
    head :ok
  end

  private

  def exchange_authorization_code
    code = params[:code].presence
    return oauth_error(:bad_request, "invalid_request", "Missing code") if code.nil?
    verifier = params[:code_verifier].presence
    return oauth_error(:bad_request, "invalid_request", "Missing code_verifier") if verifier.nil?

    row = OauthAuthorizationCode.find_by(code: code)
    return oauth_error(:bad_request, "invalid_grant", "Invalid authorization code") if row.nil?
    return oauth_error(:bad_request, "invalid_grant", "Authorization code already used") if row.used
    return oauth_error(:bad_request, "invalid_grant", "Authorization code expired") if Time.current > row.expires_at

    if params[:client_id].present? && params[:client_id] != row.client_id
      return oauth_error(:bad_request, "invalid_grant", "client_id mismatch")
    end
    if params[:redirect_uri].present? && params[:redirect_uri] != row.redirect_uri
      return oauth_error(:bad_request, "invalid_grant", "redirect_uri mismatch")
    end

    computed = Base64.urlsafe_encode64(Digest::SHA256.digest(verifier), padding: false)
    unless ActiveSupport::SecurityUtils.secure_compare(computed, row.code_challenge.to_s)
      return oauth_error(:bad_request, "invalid_grant", "PKCE verification failed")
    end

    row.update!(used: true)
    issue_token_pair(row.client_id, row.user_id, nil)
  end

  def refresh_tokens
    refresh_token = params[:refresh_token].presence
    return oauth_error(:bad_request, "invalid_request", "Missing refresh_token") if refresh_token.nil?

    token_hash = Digest::SHA256.hexdigest(refresh_token)
    row = OauthToken.find_by(token_hash: token_hash, token_type: "refresh")
    return oauth_error(:bad_request, "invalid_grant", "Invalid refresh token") if row.nil?
    return oauth_error(:bad_request, "invalid_grant", "Refresh token has been revoked") if row.revoked
    return oauth_error(:bad_request, "invalid_grant", "Refresh token expired") if Time.current > row.expires_at

    row.update!(revoked: true)
    issue_token_pair(row.client_id, row.user_id, row.id)
  end

  def issue_token_pair(client_id, user_id, parent_token_id)
    access_token = "sxat_#{SecureRandom.alphanumeric(48)}"
    refresh_token = "sxrt_#{SecureRandom.alphanumeric(48)}"

    OauthToken.create!(
      token_hash: Digest::SHA256.hexdigest(access_token), token_type: "access",
      client_id: client_id, user_id: user_id,
      expires_at: ACCESS_TOKEN_TTL.from_now, parent_token_id: parent_token_id
    )
    OauthToken.create!(
      token_hash: Digest::SHA256.hexdigest(refresh_token), token_type: "refresh",
      client_id: client_id, user_id: user_id,
      expires_at: REFRESH_TOKEN_TTL.from_now, parent_token_id: parent_token_id
    )

    render json: {
      access_token: access_token,
      token_type: "Bearer",
      expires_in: ACCESS_TOKEN_TTL.to_i,
      refresh_token: refresh_token,
      scope: "mcp"
    }
  end

  def oauth_error(status, error, description)
    render json: { error: error, error_description: description }, status: status
  end

  def base_url
    ENV.fetch("BASE_URL", "https://scrapix.meilisearch.dev")
  end

  # Matches Rust's `url::Url::parse` acceptance: an absolute URI with a scheme.
  def absolute_url?(uri)
    URI.parse(uri).scheme.present?
  rescue URI::InvalidURIError
    false
  end

  def h(text)
    ERB::Util.html_escape(text)
  end

  # The login form and error page reproduce the Rust-rendered HTML.
  def login_form_html(client_id:, redirect_uri:, code_challenge:, code_challenge_method:, state:)
    <<~HTML.chomp
      <!DOCTYPE html>
      <html lang="en">
      <head>
      <meta charset="utf-8"/>
      <meta name="viewport" content="width=device-width, initial-scale=1"/>
      <title>Sign in to Scrapix</title>
      <style>
        * { margin: 0; padding: 0; box-sizing: border-box; }
        body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
               background: #0a0a0a; color: #e5e5e5; display: flex; justify-content: center;
               align-items: center; min-height: 100vh; }
        .card { background: #171717; border: 1px solid #262626; border-radius: 12px;
                 padding: 2rem; width: 100%; max-width: 400px; }
        h1 { font-size: 1.5rem; margin-bottom: 0.5rem; }
        p { color: #a3a3a3; font-size: 0.875rem; margin-bottom: 1.5rem; }
        label { display: block; font-size: 0.875rem; margin-bottom: 0.25rem; color: #d4d4d4; }
        input { width: 100%; padding: 0.625rem; background: #0a0a0a; border: 1px solid #262626;
                border-radius: 8px; color: #e5e5e5; font-size: 0.875rem; margin-bottom: 1rem; }
        input:focus { outline: none; border-color: #6366f1; }
        button { width: 100%; padding: 0.625rem; background: #6366f1; color: white; border: none;
                 border-radius: 8px; font-size: 0.875rem; cursor: pointer; font-weight: 500; }
        button:hover { background: #4f46e5; }
        .error { color: #ef4444; font-size: 0.8rem; margin-bottom: 1rem; display: none; }
      </style>
      </head>
      <body>
      <div class="card">
        <h1>Sign in to Scrapix</h1>
        <p>An application is requesting access to your account via MCP.</p>
        <div class="error" id="error"></div>
        <form method="POST" action="/oauth/authorize">
          <input type="hidden" name="client_id" value="#{h(client_id)}"/>
          <input type="hidden" name="redirect_uri" value="#{h(redirect_uri)}"/>
          <input type="hidden" name="code_challenge" value="#{h(code_challenge)}"/>
          <input type="hidden" name="code_challenge_method" value="#{h(code_challenge_method)}"/>
          <input type="hidden" name="state" value="#{h(state)}"/>
          <label for="email">Email</label>
          <input type="email" id="email" name="email" required autocomplete="email"/>
          <label for="password">Password</label>
          <input type="password" id="password" name="password" required autocomplete="current-password"/>
          <button type="submit">Sign in &amp; Authorize</button>
        </form>
      </div>
      </body>
      </html>
    HTML
  end

  def authorize_error_page(message)
    html = <<~HTML.chomp
      <!DOCTYPE html>
      <html><head><meta charset="utf-8"/><title>Authorization Error</title>
      <style>
        body { font-family: -apple-system, sans-serif; background: #0a0a0a; color: #e5e5e5;
               display: flex; justify-content: center; align-items: center; min-height: 100vh; }
        .card { background: #171717; border: 1px solid #262626; border-radius: 12px; padding: 2rem;
                 max-width: 400px; text-align: center; }
        .error { color: #ef4444; margin-bottom: 1rem; }
        a { color: #6366f1; }
      </style></head>
      <body><div class="card">
        <p class="error">#{h(message)}</p>
        <p>Please go back and try again.</p>
      </div></body></html>
    HTML
    render html: html.html_safe, status: :bad_request
  end
end
