class EncryptMeilisearchEngineApiKeys < ActiveRecord::Migration[8.1]
  def up
    MeilisearchEngine.find_each(&:encrypt)
  end

  def down
    MeilisearchEngine.find_each(&:decrypt)
  end
end
