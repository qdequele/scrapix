# Scrapix SDKs

| SDK | Package | Directory |
|-----|---------|-----------|
| TypeScript / JavaScript | [`scrapix`](https://www.npmjs.com/package/scrapix) on npm | [`typescript/`](typescript/) |
| Python | `scrapix` on PyPI | [`python/`](python/) |

Both are thin hand-written clients over types generated from the
full-platform spec, [`contracts/openapi.json`](../contracts/openapi.json):

- TypeScript: [openapi-typescript](https://openapi-ts.dev) → `typescript/src/generated/schema.ts`
- Python: [datamodel-code-generator](https://github.com/koxudaxi/datamodel-code-generator)
  (pydantic v2) → `python/src/scrapix/_generated/models.py`

## Regenerating after a spec change

```bash
just sdk-generate   # = sdks/generate.sh
just sdk-check      # = sdks/generate.sh --check (what CI runs)
```

Generator versions are pinned (`typescript/package-lock.json`, and
`DATAMODEL_CODEGEN_VERSION` in `generate.sh`), so the output is
deterministic. Requirements: Node.js 18+ with npm, and
[uv](https://docs.astral.sh/uv/) (the Python generator runs through `uvx`).

New request fields need no client change: the TypeScript methods take the
generated request types, and the Python methods forward keyword arguments
as-is. Then run `just sdk-test`.

## Other commands

```bash
just sdk-test    # typecheck + lint + unit tests of both SDKs (mocked HTTP)
just sdk-build   # build the npm tarball and the Python sdist/wheel (no publishing)
```

Publishing is done by `.github/workflows/sdk-publish.yml`, only on a manual
run or an `sdk-typescript-v*` / `sdk-python-v*` tag.
