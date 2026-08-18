# Account/role resolution for the session-authenticated /account endpoints,
# mirroring the Rust helpers (get_user_account_id, get_user_role,
# require_role): any resolution failure surfaces as 404 "Account not found",
# insufficient role as 403 "Insufficient permissions".
module TeamContext
  extend ActiveSupport::Concern

  private

  def current_account_id!
    if @selected_account_id
      unless AccountMember.exists?(user_id: @authenticated_user_id, account_id: @selected_account_id)
        api_error!("Account not found", "not_found")
      end
      return @selected_account_id
    end

    AccountMember.where(user_id: @authenticated_user_id).limit(1).pick(:account_id) ||
      api_error!("Account not found", "not_found")
  end

  def current_role!(account_id)
    AccountMember.where(user_id: @authenticated_user_id, account_id: account_id).pick(:role) ||
      api_error!("Account not found", "not_found")
  end

  def require_role!(role, allowed)
    return if allowed.include?(role)

    api_error!("Insufficient permissions", "forbidden")
  end

  def uuid?(value)
    value.to_s.match?(/\A[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\z/i)
  end
end
