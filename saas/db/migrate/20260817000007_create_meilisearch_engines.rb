class CreateMeilisearchEngines < ActiveRecord::Migration[8.1]
  def change
    create_table :meilisearch_engines, id: :uuid do |t|
      t.references :account, type: :uuid, null: false, foreign_key: { on_delete: :cascade }
      t.text :name, null: false
      t.text :url, null: false
      t.text :api_key, null: false, default: ""
      t.boolean :is_default, null: false, default: false

      t.timestamps default: -> { "now()" }

      t.index [ :account_id, :name ], unique: true
    end
  end
end
