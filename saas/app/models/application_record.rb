class ApplicationRecord < ActiveRecord::Base
  primary_abstract_class

  # Primary keys are UUIDs (gen_random_uuid()), so id ordering is meaningless.
  self.implicit_order_column = "created_at"
end
