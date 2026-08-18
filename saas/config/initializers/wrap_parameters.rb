# Disable automatic JSON parameter wrapping. The API contract (SCR-85) uses
# flat request bodies; wrapping would inject synthetic keys named after the
# controller (e.g. a `config` key on ConfigsController requests, shadowing the
# real `config` attribute).
ActiveSupport.on_load(:action_controller) do
  wrap_parameters false
end
