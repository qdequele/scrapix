class JobsMailer < ApplicationMailer
  def completed
    @job_id = params.fetch(:job_id)
    @index_uid = params.fetch(:index_uid)
    @pages_crawled = params.fetch(:pages_crawled)
    @documents_indexed = params.fetch(:documents_indexed)
    @duration_secs = params.fetch(:duration_secs)
    mail to: params[:to],
         subject: "Crawl complete — #{@documents_indexed} documents indexed in \"#{@index_uid}\""
  end

  def failed
    @job_id = params.fetch(:job_id)
    @error_message = params.fetch(:error_message)
    @pages_crawled = params.fetch(:pages_crawled)
    mail to: params[:to], subject: "Crawl job failed — #{@job_id}"
  end
end
