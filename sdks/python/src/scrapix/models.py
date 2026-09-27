"""Pydantic models of every API schema, generated from the OpenAPI spec.

Request models (``ScrapeRequest``, ``CrawlConfig``, ...) can be passed to the
client methods instead of keyword arguments; responses are returned as these
models. Models accept unknown fields, so a newer API does not break an older
SDK. Field names that clash with pydantic (``schema``, ``from``) get a
trailing underscore (``schema_``) and keep the API name as their alias.
"""

from ._generated import models as _models
from ._generated.models import *  # noqa: F403

__all__ = sorted(
    name
    for name, value in vars(_models).items()
    if not name.startswith("_")
    and isinstance(value, type)
    and getattr(value, "__module__", "") == _models.__name__
)
