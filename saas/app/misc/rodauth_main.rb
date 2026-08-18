require "sequel/core"

class RodauthMain < Rodauth::Rails::Auth
  configure do
    # Password auth + verification + reset, TOTP 2FA with recovery codes,
    # WebAuthn passkeys (second factor and passwordless login), and social
    # login (Google/GitHub) via rodauth-omniauth.
    enable :create_account, :verify_account, :verify_account_grace_period,
      :login, :logout, :remember, :json,
      :reset_password, :reset_password_notify, :change_password, :change_password_notify,
      :otp, :recovery_codes, :webauthn, :webauthn_login,
      :omniauth, :close_account

    # Initialize Sequel and have it reuse Active Record's database connection.
    db Sequel.postgres(extensions: :activerecord_connection, keep_reference: false)
    # UUID primary keys — tokens embed the account id as a string.
    convert_token_id_to_integer? false

    # Map onto the existing users table (UUID PKs, SCR-87 I1 schema).
    accounts_table :users
    verify_account_table :user_verification_keys
    reset_password_table :user_password_reset_keys
    remember_table :user_remember_keys
    otp_keys_table :user_otp_keys
    recovery_codes_table :user_recovery_codes
    webauthn_user_ids_table :user_webauthn_user_ids
    webauthn_keys_table :user_webauthn_keys
    account_status_column :status
    account_password_hash_column :password_hash

    # Social identities live in the pre-existing oauth_identities table.
    omniauth_identities_table :oauth_identities
    omniauth_identities_account_id_column :user_id
    omniauth_identities_provider_column :provider
    omniauth_identities_uid_column :provider_user_id

    # request_path/callback_path pin the routes to /auth/<provider> —
    # without them the OmniAuth app's own path prefix stacks on Rodauth's,
    # yielding /auth/auth/<provider>.
    omniauth_provider :google_oauth2,
      ENV["GOOGLE_CLIENT_ID"].to_s, ENV["GOOGLE_CLIENT_SECRET"].to_s,
      scope: "email profile", name: :google,
      request_path: "/auth/google", callback_path: "/auth/google/callback"
    omniauth_provider :github,
      ENV["GITHUB_CLIENT_ID"].to_s, ENV["GITHUB_CLIENT_SECRET"].to_s,
      scope: "user:email",
      request_path: "/auth/github", callback_path: "/auth/github/callback"

    # GET initiation (see config/initializers/omniauth.rb) — skip the
    # POST-CSRF request validation; the callback still validates state.
    omniauth_request_validation_phase { }

    # Social login is a browser redirect flow that lands on the API host;
    # send the user back to the console afterwards.
    login_redirect { "#{ENV.fetch("CONSOLE_PUBLIC_URL", "http://localhost:3001")}/dashboard" }

    # The console is a JSON SPA; browser-driven flows (omniauth redirects,
    # email links) still get the HTML/redirect handling.
    only_json? false

    # Public routes keep their pre-Rodauth paths.
    prefix "/auth"
    create_account_route "signup"
    reset_password_request_route "forgot-password"
    reset_password_route "reset-password"
    verify_account_route "verify-email"
    verify_account_resend_route "resend-verification"

    rails_controller { RodauthController }
    title_instance_variable :@page_title

    # Set password when creating account instead of when verifying.
    verify_account_set_password? false
    # Unverified users can use the product; verification is encouraged, not
    # enforced (matches the pre-Rodauth behavior).
    verify_account_grace_period 365 * 86_400

    login_param "email"
    require_login_confirmation? false
    require_password_confirmation? false

    # Signup carries the optional display name.
    before_create_account do
      account[:full_name] = param_or_nil("full_name")
    end

    # Every user gets a personal billing account with welcome credits, and
    # any live team invites for their email are auto-accepted. Signup logs
    # the user in, so the engine JWT is issued here too. NOTE: social signups
    # go through omniauth_create_account, a SEPARATE hook — provision in both.
    after_create_account do
      issue_scrapix_session
      provision_new_user
    end
    before_omniauth_create_account { account[:full_name] = omniauth_name }
    after_omniauth_create_account { provision_new_user }

    # Welcome email shortly after the address is verified.
    after_verify_account do
      AuthMailer.with(to: account[:email], name: account[:full_name])
                .welcome.deliver_later(wait: 120.seconds)
    end

    # ==> Engine session bridge
    # The Rust engine (and our /account, /configs, ... controllers) validate a
    # stateless HS256 JWT in the scrapix_session cookie. Issue it only once
    # authentication is COMPLETE (i.e. not between MFA factors). NOTE: Rodauth
    # hooks overwrite on redefinition — keep exactly one block per hook.
    after_login do
      remember_login
      if uses_two_factor_authentication?
        # Tell the JSON client the second factor is still pending.
        json_response["two_factor_required"] = true if use_json?
      else
        issue_scrapix_session
      end
    end
    after_two_factor_authentication { issue_scrapix_session }
    after_logout { clear_scrapix_session }
    after_close_account { clear_scrapix_session }

    auth_class_eval do
      # The gem computes its roda routes from omniauth_prefix, which would
      # stack on Rodauth's prefix (/auth/auth/github). Align them with the
      # strategy-level request_path/callback_path above: /auth/<provider>.
      def omniauth_request_route(provider)
        provider.to_s
      end

      def omniauth_callback_route(provider)
        "#{provider}/callback"
      end

      def provision_new_user
        name = account[:full_name].presence || account[:email]
        billing_account = Account.create!(name: "#{name}'s Account")
        AccountMember.create!(user_id: account_id, account_id: billing_account.id, role: "owner")
        Transaction.create!(
          account_id: billing_account.id, type: "initial_deposit", amount: 100,
          balance_after: 100, description: "Welcome credit deposit"
        )
        AccountInvite.live.where(email: account[:email].to_s).find_each do |invite|
          AccountMember.create_or_find_by(user_id: account_id, account_id: invite.account_id) do |m|
            m.role = invite.role
          end
          invite.update!(status: "accepted")
        rescue ActiveRecord::ActiveRecordError => e
          Rails.logger.warn("Failed to auto-accept invite #{invite.id}: #{e.message}")
        end
      end

      def issue_scrapix_session
        rails_cookies["scrapix_session"] =
          SessionToken.cookie(SessionToken.encode(account_id, account_from_id[:email]))
      end

      def clear_scrapix_session
        # Jar clears the (possibly domain-scoped) variant; the raw header
        # clears the host-only variant — the jar allows one entry per name.
        rails_cookies["scrapix_session"] = SessionToken.clear_cookie
        if ENV["SESSION_COOKIE_DOMAIN"].present?
          rails_controller_instance.response.add_header("set-cookie", SessionToken.host_only_clear_header)
        end
      end

      def account_from_id
        account || db[accounts_table].where(account_id_column => account_id).first
      end
    end

    # ==> Emails (our ActionMailer templates; links point at the console)
    send_verify_account_email do
      AuthMailer.with(to: email_to, name: account[:full_name],
                      token: token_param_value(verify_account_key_value))
                .verification.deliver_later
    end
    send_reset_password_email do
      AuthMailer.with(to: email_to, token: token_param_value(reset_password_key_value))
                .password_reset.deliver_later
    end
    send_password_changed_email do
      AuthMailer.with(to: email_to).password_changed.deliver_later
    end
    send_reset_password_notify_email do
      AuthMailer.with(to: email_to).password_changed.deliver_later
    end

    # ==> Passwords
    password_minimum_length 12
    # bcrypt has a maximum input length of 72 bytes, truncating any extra bytes.
    password_maximum_bytes 72

    # ==> Remember Feature (remember_login runs in the after_login hook above)
    extend_remember_deadline? true

    # ==> MFA
    # Generate recovery codes automatically when a second factor is added.
    auto_add_recovery_codes? true

    # ==> WebAuthn
    webauthn_rp_name "Scrapix"
    # Ceremonies run in the browser on the console origin, but the setup
    # request reaches Rails through the console proxy (Host: 127.0.0.1), so
    # the request-derived RP ID would never match the browser's domain
    # ("relying party ID is not a registrable domain suffix..."). Pin both
    # to the public console domain; the parent domain also covers the API
    # subdomain for the webauthn-login flow.
    if (console_url = ENV["CONSOLE_PUBLIC_URL"]).present?
      webauthn_rp_id URI(console_url).host
      webauthn_origin console_url
    end

    # ==> Deadlines
    verify_account_skip_resend_email_within 0 # resend allowed anytime
    remember_deadline_interval days: 30
  end
end
