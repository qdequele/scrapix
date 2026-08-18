# The social login buttons are plain links (top-level GET navigations).
# OmniAuth 2 defaults to POST-only initiation as CSRF hardening; login
# initiation forgery is an accepted risk here (same as the pre-Rodauth
# implementation), and the callback phase keeps full state validation.
OmniAuth.config.allowed_request_methods = %i[get post]
OmniAuth.config.silence_get_warning = true
