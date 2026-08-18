class BillingMailer < ApplicationMailer
  def payment_receipt
    @credits = params.fetch(:credits)
    @amount_cents = params.fetch(:amount_cents)
    mail to: params[:to], subject: "Payment receipt — #{@credits} credits"
  end

  def auto_topup_receipt
    @credits = params.fetch(:credits)
    @amount_cents = params.fetch(:amount_cents)
    @new_balance = params.fetch(:new_balance)
    mail to: params[:to], subject: "Auto top-up — #{@credits} credits added"
  end

  def auto_topup_failed
    @reason = params.fetch(:reason)
    mail to: params[:to], subject: "Action required — auto top-up failed"
  end

  def low_balance
    @balance = params.fetch(:balance)
    mail to: params[:to], subject: "Low balance — #{@balance} credits remaining"
  end
end
