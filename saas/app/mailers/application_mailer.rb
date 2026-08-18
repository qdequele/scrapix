class ApplicationMailer < ActionMailer::Base
  default from: "Scrapix <noreply@scrapix.meilisearch.com>"
  layout "mailer"
  helper MailerHelper
end
