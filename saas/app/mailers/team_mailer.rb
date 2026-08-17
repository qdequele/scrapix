class TeamMailer < ApplicationMailer
  def invite
    @account_name = params.fetch(:account_name)
    @inviter_name = params.fetch(:inviter_name)
    @role = params.fetch(:role)
    @token = params.fetch(:token)
    mail to: params[:to], subject: "You're invited to join #{@account_name} on Scrapix"
  end

  def invite_accepted
    @member_name = params.fetch(:member_name)
    @account_name = params.fetch(:account_name)
    @role = params.fetch(:role)
    mail to: params[:to], subject: "#{@member_name} joined #{@account_name}"
  end

  def member_removed
    @account_name = params.fetch(:account_name)
    @removed_by = params.fetch(:removed_by)
    mail to: params[:to], subject: "You've been removed from #{@account_name}"
  end
end
