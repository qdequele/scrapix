# Resend's sandbox rejects placeholder domains, so contract-test signups to
# @example.com pile up as failed SolidQueue executions in dev. Skip actual
# SMTP delivery for reserved test domains in development only (the :test
# delivery method used by the minitest suite is unaffected).
class DevRecipientFilter
  RESERVED = /@(example\.(com|org|net)|test)\z/i

  def self.delivering_email(mail)
    if Array(mail.to).all? { |to| to.match?(RESERVED) }
      mail.perform_deliveries = false
      Rails.logger.info("Skipping delivery to reserved test domain: #{mail.to.join(', ')}")
    end
  end
end
