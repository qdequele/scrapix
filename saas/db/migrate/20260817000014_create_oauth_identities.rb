class CreateOauthIdentities < ActiveRecord::Migration[8.1]
  def change
    # Social login links (Google, GitHub).
    create_table :oauth_identities, id: :uuid do |t|
      t.references :user, type: :uuid, null: false, foreign_key: { on_delete: :cascade }
      t.text :provider, null: false
      t.text :provider_user_id, null: false
      t.timestamptz :created_at, null: false, default: -> { "now()" }

      t.check_constraint "provider IN ('google', 'github')", name: "oauth_identities_provider_check"
      t.index [ :provider, :provider_user_id ], unique: true
    end
  end
end
