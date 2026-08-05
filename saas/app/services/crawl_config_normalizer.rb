# Normalizes a crawl config the way the Rust API does when it round-trips the
# JSON through the `CrawlConfig` serde struct: missing fields are filled with
# defaults, unknown fields are dropped.
#
# The defaults skeleton (config/crawl_config_defaults.json) was captured from
# the live Rust API by creating a config with only the required fields — it is
# the serialized form of `CrawlConfig::default()`. Regenerate it against the
# Rust API if the struct changes.
#
# Merge rules per skeleton value:
# - object with keys  -> recurse; user keys not in the skeleton are dropped
# - empty object ({}) -> serde map (e.g. headers): user value passes through
# - anything else     -> user value if the key is present, else the default
#
# Known limit: a partial object for a defaults-to-null field (e.g. `proxy`)
# passes through unfilled; the Rust engine fills those defaults at read time.
class CrawlConfigNormalizer
  SKELETON = JSON.parse(
    Rails.root.join("config/crawl_config_defaults.json").read
  ).freeze

  def self.normalize(config)
    merge(config, SKELETON)
  end

  def self.merge(user, skeleton)
    return user unless skeleton.is_a?(Hash)
    return user if skeleton.empty? # serde map: arbitrary keys allowed

    skeleton.each_with_object({}) do |(key, default), out|
      if user.is_a?(Hash) && user.key?(key)
        value = user[key]
        out[key] = default.is_a?(Hash) && value.is_a?(Hash) ? merge(value, default) : value
      else
        out[key] = default
      end
    end
  end
  private_class_method :merge
end
