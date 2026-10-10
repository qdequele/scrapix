# Kubernetes manifests (hosted engine)

`base/` deploys the Scrapix engine in **hosted** mode (`SCRAPIX_MODE=hosted`
on the API): it keeps its own `scrapix_engine` Postgres and talks to the
Lab (`meilisearch/lab`, deployed separately) over `LAB_URL`. To self-host
without the Lab, patch the API to `SCRAPIX_MODE=standalone` with
`SCRAPIX_ADMIN_KEY` and drop the `LAB_*` values (see
`docs/deployment/self-hosting.mdx`).

## Secrets the API needs

| Key | Where it comes from |
|-----|---------------------|
| `LAB_SERVICE_TOKEN` | Generate it first: `openssl rand -hex 32`. The Lab presents it when it calls this engine for an account (saved-config cron); it is passed to the Lab at mint time as `CREDENTIAL=` (the Lab stores it per engine). |
| `LAB_INSTANCE_ID`, `LAB_INSTANCE_SECRET` | Minted by the Lab for this engine deployment: on the Lab, run `bin/rails lab:hosted_engine:create PRODUCT=scrapix REGION=<region> URL=<this API's public URL> CREDENTIAL=<LAB_SERVICE_TOKEN>`; it prints `LAB_URL`, `LAB_INSTANCE_ID` and `LAB_INSTANCE_SECRET` (64 hex) once. Rotate the secret with `bin/rails lab:hosted_engine:rotate ID=<LAB_INSTANCE_ID>`; the old secret stays valid for 10 minutes. |
| `MEILISEARCH_API_KEY`, `POSTGRES_PASSWORD`, `CLICKHOUSE_PASSWORD` | Your infrastructure. |

`LAB_URL` lives in the ConfigMap (`base/config/configmap.yaml`): the Lab's
public base URL, the `LAB_URL` the mint task printed.

```bash
LAB_SERVICE_TOKEN=$(openssl rand -hex 32)
# on the Lab: bin/rails lab:hosted_engine:create PRODUCT=scrapix REGION=<region> \
#   URL=<this API's public URL> CREDENTIAL=$LAB_SERVICE_TOKEN
kubectl -n scrapix create secret generic scrapix-secrets \
  --from-literal=LAB_INSTANCE_ID=<LAB_INSTANCE_ID printed by the Lab> \
  --from-literal=LAB_INSTANCE_SECRET=<LAB_INSTANCE_SECRET printed by the Lab> \
  --from-literal=LAB_SERVICE_TOKEN=$LAB_SERVICE_TOKEN \
  --from-literal=MEILISEARCH_API_KEY=... \
  --from-literal=POSTGRES_PASSWORD=... \
  --from-literal=CLICKHOUSE_PASSWORD=...
```

The API refuses to start when `LAB_INSTANCE_ID` is not a uuid, when
`LAB_INSTANCE_SECRET` is not 64 hex characters, when the Lab rejects them,
or when `GET {LAB_URL}/internal/instances/me` is missing (a Lab that predates
contract v2 or a wrong `LAB_URL`).
