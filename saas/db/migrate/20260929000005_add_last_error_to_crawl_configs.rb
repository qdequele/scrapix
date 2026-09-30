class AddLastErrorToCrawlConfigs < ActiveRecord::Migration[8.1]
  def change
    add_column :crawl_configs, :last_error, :text
  end
end
