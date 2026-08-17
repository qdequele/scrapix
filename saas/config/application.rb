require_relative "boot"

# Load only the frameworks the SaaS API uses. Action Mailer stays for the
# auth/email phases; Active Storage, Action Text, Action Mailbox, and Action
# Cable are intentionally excluded (real-time stays on the Rust engine).
require "rails"
require "active_model/railtie"
require "active_job/railtie"
require "active_record/railtie"
require "action_controller/railtie"
require "action_mailer/railtie"

# Require the gems listed in Gemfile, including any gems
# you've limited to :test, :development, or :production.
Bundler.require(*Rails.groups)

module Saas
  class Application < Rails::Application
    # Initialize configuration defaults for originally generated Rails version.
    config.load_defaults 8.1

    # Please, add to the `ignore` list any other `lib` subdirectories that do
    # not contain `.rb` files, or that should not be reloaded or eager loaded.
    # Common ones are `templates`, `generators`, or `middleware`, for example.
    config.autoload_lib(ignore: %w[assets tasks])

    # Configuration for the application, engines, and railties goes here.
    #
    # These settings can be overridden in specific environments using the files
    # in config/environments, which are processed later.
    #
    # config.time_zone = "Central Time (US & Canada)"
    # config.eager_load_paths << Rails.root.join("extras")

    # Only loads a smaller set of middleware suitable for API only apps.
    # Middleware like session, flash, cookies can be added back manually.
    # Skip views, helpers and assets when generating a new resource.
    config.api_only = true

    # The auth endpoints set/clear the scrapix_session cookie; API mode
    # excludes the cookie middleware that serializes the jar into headers.
    config.middleware.use ActionDispatch::Cookies

    # The schema carries a PL/pgSQL function (validate_api_key, shared with
    # the Rust engine) and partial indexes — schema.rb can't express those.
    config.active_record.schema_format = :sql
  end
end
