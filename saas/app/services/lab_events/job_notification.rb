module LabEvents
  # job.completed / job.failed → one email per job to the account owner, if
  # they opted in (users.notify_job_emails).
  class JobNotification
    TYPES = { "job.completed" => "job_completed", "job.failed" => "job_failed" }.freeze

    def self.call(event)
      account = Account.find_by(id: event.account_id)
      return Processor.reject(event, "unknown account #{event.account_id}") unless account

      owner = account.account_members.find_by(role: "owner")&.user
      return unless owner&.notify_job_emails

      ScheduledEmail.create!(email_type: TYPES.fetch(event.type), recipient: owner.email, payload: event.data, send_at: Time.current)
    rescue ActiveRecord::RecordNotUnique
      nil # one email per job (index_scheduled_emails_job_dedupe)
    end
  end
end
