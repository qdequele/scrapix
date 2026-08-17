# Rodauth support on the existing UUID `users` table: status column +
# citext email, per-feature key tables (verification, reset, login change,
# remember), and MFA tables (TOTP, recovery codes, WebAuthn).
#
# Replaces the hand-rolled auth columns: email_verified/
# email_verification_token and the password_reset_tokens table.
class CreateRodauth < ActiveRecord::Migration[8.1]
  def change
    enable_extension "citext"

    # Rodauth account state: 1=unverified, 2=verified, 3=closed.
    add_column :users, :status, :integer, null: false, default: 1
    change_column :users, :email, :citext, null: false

    # State now lives in `status` / user_verification_keys.
    remove_column :users, :email_verified, :boolean, null: false, default: false
    remove_column :users, :email_verification_token, :text
    drop_table :password_reset_tokens do |t|
      t.uuid :user_id, null: false
      t.text :token_hash, null: false
      t.timestamptz :expires_at, null: false
      t.boolean :used, null: false, default: false
      t.timestamptz :created_at, null: false
    end

    # Used by the password reset feature
    create_table :user_password_reset_keys, id: false do |t|
      t.uuid :id, primary_key: true
      t.foreign_key :users, column: :id
      t.string :key, null: false
      t.datetime :deadline, null: false
      t.datetime :email_last_sent, null: false, default: -> { "CURRENT_TIMESTAMP" }
    end

    # Used by the account verification feature
    create_table :user_verification_keys, id: false do |t|
      t.uuid :id, primary_key: true
      t.foreign_key :users, column: :id
      t.string :key, null: false
      t.datetime :requested_at, null: false, default: -> { "CURRENT_TIMESTAMP" }
      t.datetime :email_last_sent, null: false, default: -> { "CURRENT_TIMESTAMP" }
    end

    # Used by the verify login change feature
    create_table :user_login_change_keys, id: false do |t|
      t.uuid :id, primary_key: true
      t.foreign_key :users, column: :id
      t.string :key, null: false
      t.string :login, null: false
      t.datetime :deadline, null: false
    end

    # Used by the remember me feature
    create_table :user_remember_keys, id: false do |t|
      t.uuid :id, primary_key: true
      t.foreign_key :users, column: :id
      t.string :key, null: false
      t.datetime :deadline, null: false
    end

    # Used by the TOTP feature
    create_table :user_otp_keys, id: false do |t|
      t.uuid :id, primary_key: true
      t.foreign_key :users, column: :id
      t.string :key, null: false
      t.integer :num_failures, null: false, default: 0
      t.datetime :last_use, null: false, default: -> { "CURRENT_TIMESTAMP" }
    end

    # Used by the recovery codes feature
    create_table :user_recovery_codes, primary_key: [ :id, :code ] do |t|
      t.uuid :id
      t.foreign_key :users, column: :id
      t.string :code
    end

    # Used by the WebAuthn feature
    create_table :user_webauthn_user_ids, id: false do |t|
      t.uuid :id, primary_key: true
      t.foreign_key :users, column: :id
      t.string :webauthn_id, null: false
    end
    create_table :user_webauthn_keys, primary_key: [ :account_id, :webauthn_id ] do |t|
      t.uuid :account_id
      t.foreign_key :users, column: :account_id
      t.string :webauthn_id
      t.string :public_key, null: false
      t.integer :sign_count, null: false
      t.datetime :last_use, null: false, default: -> { "CURRENT_TIMESTAMP" }
    end
  end
end
