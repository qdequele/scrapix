class CreateOauthProviderTables < ActiveRecord::Migration[8.1]
  def change
    # OAuth 2.1 provider (RFC 7591 dynamic registration, PKCE, rotation).
    # Rails issues everything; the Rust engine validates access-token hashes.
    create_table :oauth_clients, id: :uuid do |t|
      t.string :client_id, limit: 64, null: false, index: { unique: true }
      t.string :client_name, limit: 255
      t.text :redirect_uris, array: true, null: false
      t.string :scope, limit: 255, default: "mcp"
      t.timestamptz :created_at, null: false, default: -> { "now()" }
    end

    create_table :oauth_authorization_codes, id: :string, primary_key: :code, limit: 128 do |t|
      t.string :client_id, limit: 64, null: false
      t.uuid :user_id, null: false
      t.text :redirect_uri, null: false
      t.string :scope, limit: 255, default: "mcp"
      t.string :code_challenge, limit: 128, null: false
      t.string :code_challenge_method, limit: 10, null: false, default: "S256"
      t.timestamptz :expires_at, null: false, index: true
      t.boolean :used, null: false, default: false
      t.timestamptz :created_at, null: false, default: -> { "now()" }
    end
    add_foreign_key :oauth_authorization_codes, :oauth_clients, column: :client_id, primary_key: :client_id
    add_foreign_key :oauth_authorization_codes, :users

    create_table :oauth_tokens, id: :uuid do |t|
      t.string :token_hash, limit: 64, null: false, index: { unique: true }
      t.string :token_type, limit: 16, null: false
      t.string :client_id, limit: 64, null: false
      t.uuid :user_id, null: false
      t.string :scope, limit: 255, default: "mcp"
      t.timestamptz :expires_at, null: false
      t.boolean :revoked, null: false, default: false
      t.uuid :parent_token_id
      t.timestamptz :created_at, null: false, default: -> { "now()" }

      t.check_constraint "token_type IN ('access', 'refresh')", name: "oauth_tokens_token_type_check"

      # The hot lookup path (engine + Rails Bearer validation).
      t.index :token_hash, where: "revoked = false", name: "index_oauth_tokens_on_token_hash_live"
    end
    add_foreign_key :oauth_tokens, :oauth_clients, column: :client_id, primary_key: :client_id
    add_foreign_key :oauth_tokens, :users
    add_foreign_key :oauth_tokens, :oauth_tokens, column: :parent_token_id
  end
end
