# In-process CSRF state store for the social OAuth flow, matching the Rust
# implementation's semantics (bins/scrapix-api/src/auth/social.rs): entries
# expire after 10 minutes and are single-use. Single-instance only — the same
# limitation the Rust store has.
class OauthStateStore
  TTL = 600 # seconds

  @states = {}
  @mutex = Mutex.new

  class << self
    def insert(state, provider, callback_uri)
      @mutex.synchronize do
        now = Process.clock_gettime(Process::CLOCK_MONOTONIC)
        @states.delete_if { |_, (_, _, at)| now - at > TTL }
        @states[state] = [ provider, callback_uri, now ]
      end
    end

    def take(state)
      @mutex.synchronize do
        provider, callback_uri, at = @states.delete(state)
        return nil unless provider
        return nil if Process.clock_gettime(Process::CLOCK_MONOTONIC) - at > TTL

        [ provider, callback_uri ]
      end
    end
  end
end
