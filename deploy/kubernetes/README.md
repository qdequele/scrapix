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
| `LAB_INSTANCE_ID`, `LAB_INSTANCE_SECRET` | Minted by the Lab for this engine deployment: on the Lab, run `bin/rails lab:hosted_engine:create PRODUCT=scrapix REGION=<region> URL=<this API's public URL>`; it prints `instance_id`, `secret` (64 hex, shown once) and `lab_url`. Rotate with the same task; the old secret stays valid for 10 minutes. |
| `LAB_SERVICE_TOKEN` | `openssl rand -hex 32`; the same value on the Lab (`LAB_SERVICE_TOKEN`). The Lab presents it when it calls this engine for an account (saved-config cron). |
| `MEILISEARCH_API_KEY`, `POSTGRES_PASSWORD`, `CLICKHOUSE_PASSWORD` | Your infrastructure. |

`LAB_URL` lives in the ConfigMap (`base/config/configmap.yaml`): the Lab's
public base URL, the `lab_url` the mint task printed.

```bash
kubectl -n scrapix create secret generic scrapix-secrets \
  --from-literal=LAB_INSTANCE_ID=<instance_id> \
  --from-literal=LAB_INSTANCE_SECRET=<secret> \
  --from-literal=LAB_SERVICE_TOKEN=$(openssl rand -hex 32) \
  --from-literal=MEILISEARCH_API_KEY=... \
  --from-literal=POSTGRES_PASSWORD=... \
  --from-literal=CLICKHOUSE_PASSWORD=...
```

The API refuses to start when `LAB_INSTANCE_ID` is not a uuid, when
`LAB_INSTANCE_SECRET` is not 64 hex characters, when the Lab rejects them,
or when `GET {LAB_URL}/internal/instances/me` is missing (a Lab that predates
contract v2 or a wrong `LAB_URL`).
