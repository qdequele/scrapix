# Queues transactional emails in the shared `scheduled_emails` table. The Rust
# engine's email scheduler polls this table and does the actual Resend
# delivery, so Rails never talks to the email provider directly. Payload
# shapes must match bins/scrapix-api/src/email_scheduler.rs.
module EmailQueue
  def self.enqueue(email_type, recipient, payload = {}, send_at: Time.current)
    ActiveRecord::Base.connection.execute(
      ActiveRecord::Base.sanitize_sql_array([
        "INSERT INTO scheduled_emails (email_type, recipient, payload, send_at) VALUES (?, ?, ?::jsonb, ?)",
        email_type, recipient, payload.to_json, send_at
      ])
    )
  rescue ActiveRecord::ActiveRecordError => e
    Rails.logger.warn("Failed to queue #{email_type} email: #{e.message}")
  end

  def self.verification(email, name, token)
    enqueue("verification", email, { name: name.to_s, token: token })
  end

  def self.password_reset(email, token)
    enqueue("password_reset", email, { token: token })
  end

  def self.password_changed(email)
    enqueue("password_changed", email)
  end

  def self.welcome(email, name, send_at: Time.current)
    enqueue("welcome", email, { name: name.to_s }, send_at: send_at)
  end
end
