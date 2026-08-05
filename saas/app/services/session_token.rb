# Session JWTs identical to the Rust engine's (crates/scrapix-auth/src/jwt.rs):
# HS256, claims {sub: user_id, email, exp: now+7d, iat}, signed with JWT_SECRET.
# The Rust middleware keeps validating these unchanged — session compatibility
# is the linchpin of the SCR-85 cutover.
module SessionToken
  VALIDITY = 7.days

  def self.encode(user_id, email)
    now = Time.current.to_i
    JWT.encode(
      { sub: user_id.to_s, email: email, exp: now + VALIDITY.to_i, iat: now },
      ENV.fetch("JWT_SECRET"),
      "HS256"
    )
  end

  # Cookie attributes matching the Rust build_session_cookie: path /, HttpOnly,
  # SameSite=Lax, Secure outside development, 7-day expiry.
  def self.cookie(value)
    {
      value: value,
      path: "/",
      httponly: true,
      same_site: :lax,
      secure: ENV.fetch("ENVIRONMENT", "production") != "development",
      expires: VALIDITY.from_now
    }
  end

  def self.clear_cookie
    cookie("").merge(expires: Time.at(0))
  end
end
