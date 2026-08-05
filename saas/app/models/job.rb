# Crawl jobs are owned by the Rust engine (SCR-85 table ownership); Rails
# reads them for display/reporting only and must never write.
class Job < ApplicationRecord
  self.primary_key = "job_id"

  belongs_to :account, optional: true
  belongs_to :api_key, optional: true

  def readonly?
    true
  end
end
