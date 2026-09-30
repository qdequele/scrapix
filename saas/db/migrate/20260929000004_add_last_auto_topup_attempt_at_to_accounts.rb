# AutoTopup's cooldown: set when a charge attempt starts, cleared when it
# succeeds. A value younger than AutoTopup::RETRY_AFTER is an attempt that
# failed or whose outcome is unknown, and blocks the next attempt.
class AddLastAutoTopupAttemptAtToAccounts < ActiveRecord::Migration[8.1]
  def change
    add_column :accounts, :last_auto_topup_attempt_at, :timestamptz
  end
end
