# Outbound email via Resend SMTP. Without RESEND_API_KEY, deliveries go to
# the :test delivery method (captured in ActionMailer::Base.deliveries) so
# dev and CI never need credentials.
Rails.application.configure do
  if ENV["RESEND_API_KEY"].present?
    config.action_mailer.delivery_method = :smtp
    config.action_mailer.smtp_settings = {
      address: "smtp.resend.com",
      # Scaleway (and other hosts) block egress on 465/587; Resend also
      # listens on 2465 (SMTPS) for exactly this case.
      port: ENV.fetch("RESEND_SMTP_PORT", 465).to_i,
      user_name: "resend",
      password: ENV["RESEND_API_KEY"],
      tls: true
    }
  else
    config.action_mailer.delivery_method = :test
  end
  config.action_mailer.raise_delivery_errors = true
end

# App classes aren't autoloadable at initializer time — defer to to_prepare.
Rails.application.config.to_prepare do
  ActionMailer::Base.register_interceptor(DevRecipientFilter) if Rails.env.development?
end
