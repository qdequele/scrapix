class CreateApiKeys < ActiveRecord::Migration[8.1]
  def change
    create_table :api_keys, id: :uuid do |t|
      t.references :account, type: :uuid, null: false, foreign_key: { on_delete: :cascade }
      t.text :name, null: false
      t.text :prefix, null: false
      t.text :key_hash, null: false, index: true
      t.boolean :active, null: false, default: true
      t.timestamptz :last_used_at
      t.timestamptz :created_at, null: false, default: -> { "now()" }
    end

    # Shared with the Rust engine's auth middleware: validates a key hash and
    # bumps last_used_at in one round-trip. Both backends call
    # SELECT ... FROM validate_api_key($1).
    reversible do |dir|
      dir.up do
        execute <<~SQL
          CREATE OR REPLACE FUNCTION validate_api_key(p_key_hash TEXT)
          RETURNS TABLE (account_id UUID, tier TEXT, active BOOLEAN, api_key_id UUID) AS $$
          BEGIN
              RETURN QUERY
              SELECT a.id AS account_id, a.tier, a.active, k.id AS api_key_id
              FROM api_keys k
              JOIN accounts a ON a.id = k.account_id
              WHERE k.key_hash = p_key_hash
                AND k.active = true
                AND a.active = true;

              -- Update last_used_at
              UPDATE api_keys SET last_used_at = now() WHERE api_keys.key_hash = p_key_hash AND api_keys.active = true;
          END;
          $$ LANGUAGE plpgsql;
        SQL
      end
      dir.down do
        execute "DROP FUNCTION IF EXISTS validate_api_key(TEXT)"
      end
    end
  end
end
