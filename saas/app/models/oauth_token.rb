# OAuth access/refresh token, stored as a SHA-256 hash. Access tokens live
# 1 hour, refresh tokens 30 days and rotate on use.
class OauthToken < ApplicationRecord
end
