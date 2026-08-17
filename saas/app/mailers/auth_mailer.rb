class AuthMailer < ApplicationMailer
  def welcome
    @name = params[:name].presence || "there"
    mail to: params[:to], subject: "Welcome to Scrapix — your 100 free credits are ready"
  end

  def verification
    @name = params[:name].presence || "there"
    @token = params.fetch(:token)
    mail to: params[:to], subject: "Verify your email address — Scrapix"
  end

  def password_reset
    @token = params.fetch(:token)
    mail to: params[:to], subject: "Reset your password — Scrapix"
  end

  def password_changed
    mail to: params[:to], subject: "Your password has been changed — Scrapix"
  end
end
