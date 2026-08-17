class CreateUsers < ActiveRecord::Migration[8.1]
  def change
    create_table :users, id: :uuid do |t|
      t.text :email, null: false, index: { unique: true }
      # Nullable: social-login-only users have no password.
      t.text :password_hash
      t.text :full_name
      t.boolean :email_verified, null: false, default: false
      t.text :email_verification_token
      t.boolean :notify_job_emails, null: false, default: true

      t.timestamps default: -> { "now()" }
    end
  end
end
