# MCP Streamable HTTP endpoint — replaces bins/scrapix-api/src/mcp.rs.
#
# OAuth 2.1 Bearer auth (same 401 bodies as the Rust validate_mcp_bearer
# middleware), then JSON-RPC handling via the official MCP Ruby SDK. Responses
# are plain application/json; we don't offer a server-initiated stream, so GET
# and DELETE return 405 as the Streamable HTTP spec allows for stateless
# servers.
class McpController < ApplicationController
  def handle
    return head :method_not_allowed unless request.post?

    token = bearer_token
    if token.nil?
      return render json: { error: "Missing Bearer token", code: "missing_token" }, status: :unauthorized
    end
    unless valid_bearer?(token)
      return render json: { error: "Invalid or expired token", code: "invalid_token" }, status: :unauthorized
    end

    server = MCP::Server.new(
      name: "scrapix",
      version: "1.0.0",
      tools: McpToolset.tools,
      server_context: { bearer: token }
    )
    response_json = server.handle_json(request.raw_post)

    if response_json.nil?
      head :accepted # notification — no JSON-RPC response
    else
      render json: response_json
    end
  end

  private

  def bearer_token
    request.headers["Authorization"]&.delete_prefix("Bearer ").presence if
      request.headers["Authorization"]&.start_with?("Bearer ")
  end

  # Same lookup as auth/oauth.rs validate_bearer_token: an unexpired,
  # unrevoked access token whose user has at least one account membership.
  def valid_bearer?(token)
    token_hash = Digest::SHA256.hexdigest(token)
    row = ActiveRecord::Base.connection.select_one(
      ActiveRecord::Base.sanitize_sql_array([ <<~SQL, token_hash ])
        SELECT t.expires_at, t.revoked
        FROM oauth_tokens t
        JOIN account_members m ON m.user_id = t.user_id
        WHERE t.token_hash = ? AND t.token_type = 'access'
        LIMIT 1
      SQL
    )
    row.present? && !row["revoked"] && Time.current <= row["expires_at"]
  end
end
