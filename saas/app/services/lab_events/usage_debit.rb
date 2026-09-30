module LabEvents
  # usage.recorded → one usage_deduction per event (idempotent on the row id),
  # a low-balance email when that debit crosses LOW_BALANCE, then auto top-up.
  class UsageDebit
    LOW_BALANCE = 10

    def self.call(event)
      account = Account.find_by(id: event.account_id)
      return Processor.reject(event, "unknown account #{event.account_id}") unless account

      d = event.data.is_a?(Hash) ? event.data : {}
      credits = d["credits"]
      operation = d["operation"]
      unless credits.is_a?(Integer) && credits >= 0
        return Processor.reject(event, "invalid credits #{credits.inspect.truncate(100)} (want an integer >= 0)")
      end
      unless operation.is_a?(String) && operation.present?
        return Processor.reject(event, "invalid operation #{operation.inspect.truncate(100)} (want a non-empty string)")
      end

      result = account.debit_usage!(credits, lab_event_id: event.id, operation: operation,
                                    description: d["description"].to_s)
      if result
        before, after = result
        if before > LOW_BALANCE && after <= LOW_BALANCE && (to = account.owner_email)
          ScheduledEmail.create!(email_type: "low_balance", recipient: to, payload: { balance: after }, send_at: Time.current)
        end
      end
      # Also when already debited (a retry): the previous run may have raised
      # or crashed after the debit committed but before the top-up. Repeating
      # it is safe — AutoTopup re-checks the balance and its claim under the lock.
      AutoTopup.call(account)
    end
  end
end
