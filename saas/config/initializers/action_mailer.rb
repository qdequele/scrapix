# Outbound email via Resend SMTP. Without RESEND_API_KEY, deliveries go to
# the :test delivery method (captured in ActionMailer::Base.deliveries) so
# dev and CI never need credentials.
Rails.application.configure do
  if ENV["RESEND_API_KEY"].present?
    config.action_mailer.delivery_method = :smtp
    config.action_mailer.smtp_settings = {
      address: "smtp.resend.com",
      port: 465,
      user_name: "resend",
      password: ENV["RESEND_API_KEY"],
      tls: true
    }
  else
    config.action_mailer.delivery_method = :test
  end
  config.action_mailer.raise_delivery_errors = true
end
