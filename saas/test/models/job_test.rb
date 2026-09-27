require "test_helper"

class JobTest < ActiveSupport::TestCase
  test "jobs.accounting is a non-null jsonb column defaulting to {}" do
    column = Job.columns_hash["accounting"]

    assert column, "jobs.accounting column is missing"
    assert_equal :jsonb, column.type
    assert_not column.null
    assert_equal({}, Job.column_defaults["accounting"])
  end
end
