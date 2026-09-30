module Internal
  class PingController < ApplicationController
    include ServiceAuthentication

    def show = render(json: { ok: true })
  end
end
