"use client";

import { useState } from "react";
import Link from "next/link";
import { Controller, useForm, useWatch } from "react-hook-form";
import { zodResolver } from "@hookform/resolvers/zod";
import { z } from "zod";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";
import { ArrowRight, Files, Loader2, Square, XCircle } from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Progress } from "@/components/ui/progress";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import { ToggleGroup, ToggleGroupItem } from "@/components/ui/toggle-group";
import { CodeBlock } from "@/app/dashboard/playground/result-panel";
import { JobResults, jobResultsQueryKey } from "@/app/dashboard/jobs/job-results";
import {
  ApiRequestError,
  cancelJob,
  fetchJobStatus,
  submitBatchScrape,
} from "@/lib/api";
import type { BatchScrapeRequest } from "@/lib/api-types";

const MAX_URLS = 1000;
const MAX_CONCURRENCY = 25;
const TERMINAL = ["completed", "failed", "cancelled"];

const FORMATS = [
  { value: "markdown", label: "Markdown" },
  { value: "html", label: "HTML" },
  { value: "rawhtml", label: "Raw HTML" },
  { value: "content", label: "Content" },
  { value: "links", label: "Links" },
  { value: "metadata", label: "Metadata" },
] as const;
type Format = (typeof FORMATS)[number]["value"];
const FORMAT_VALUES = FORMATS.map((f) => f.value) as [Format, ...Format[]];

function parseUrls(text: string): string[] {
  return text
    .split("\n")
    .map((u) => u.trim())
    .filter(Boolean);
}

function isHttpUrl(value: string): boolean {
  try {
    const url = new URL(value);
    return url.protocol === "http:" || url.protocol === "https:";
  } catch {
    return false;
  }
}

const batchFormSchema = z
  .object({
    urls: z.string().trim().min(1, "Enter at least one URL"),
    formats: z.array(z.enum(FORMAT_VALUES)).min(1, "Select at least one format"),
    only_main_content: z.boolean(),
    render_js: z.boolean(),
    concurrency: z
      .number({ error: "Enter a number" })
      .int("Must be a whole number")
      .min(1, "At least 1")
      .max(MAX_CONCURRENCY, `At most ${MAX_CONCURRENCY}`),
  })
  .superRefine((values, ctx) => {
    const lines = values.urls.split("\n");
    const urls = parseUrls(values.urls);
    if (urls.length > MAX_URLS) {
      ctx.addIssue({
        code: "custom",
        path: ["urls"],
        message: `At most ${MAX_URLS} URLs (got ${urls.length})`,
      });
      return;
    }
    const badLine = lines.findIndex((l) => l.trim() && !isHttpUrl(l.trim()));
    if (badLine !== -1) {
      ctx.addIssue({
        code: "custom",
        path: ["urls"],
        message: `Line ${badLine + 1} is not an http(s) URL: ${lines[badLine].trim()}`,
      });
    }
  });

type BatchFormValues = z.infer<typeof batchFormSchema>;

const DEFAULT_VALUES: BatchFormValues = {
  urls: "https://scrapix.meilisearch.dev\nhttps://scrapix.meilisearch.dev/pricing",
  formats: ["markdown", "metadata"],
  only_main_content: true,
  render_js: false,
  concurrency: 10,
};

function toRequest(values: BatchFormValues): BatchScrapeRequest {
  return {
    urls: parseUrls(values.urls),
    formats: values.formats,
    only_main_content: values.only_main_content,
    render_js: values.render_js,
    concurrency: values.concurrency,
  };
}

function curlExample(request: BatchScrapeRequest): string {
  const body = { ...request, urls: request.urls.slice(0, 3) };
  return `curl -X POST https://scrapix.meilisearch.dev/batch/scrape \\
  -H "Content-Type: application/json" \\
  -H "Authorization: Bearer YOUR_API_KEY" \\
  -d '${JSON.stringify(body)}'

# then poll the job and page through its results
curl https://scrapix.meilisearch.dev/job/JOB_ID/status \\
  -H "Authorization: Bearer YOUR_API_KEY"
curl "https://scrapix.meilisearch.dev/job/JOB_ID/results?limit=20" \\
  -H "Authorization: Bearer YOUR_API_KEY"`;
}

function errorMessage(err: Error): string {
  if (err instanceof ApiRequestError && err.body) return err.body.error;
  return err.message;
}

function StatusBadge({ status }: { status: string }) {
  const variant =
    status === "failed" || status === "cancelled"
      ? "destructive"
      : status === "completed"
        ? "default"
        : "secondary";
  return (
    <Badge variant={variant} className="capitalize">
      {!TERMINAL.includes(status) && (
        <Loader2 className="mr-1 h-3 w-3 animate-spin" />
      )}
      {status}
    </Badge>
  );
}

export default function BatchScrapePage() {
  const queryClient = useQueryClient();
  const [jobId, setJobId] = useState<string | null>(null);
  const [submittedCount, setSubmittedCount] = useState(0);
  const [lastRequest, setLastRequest] = useState<BatchScrapeRequest>(
    toRequest(DEFAULT_VALUES),
  );

  const form = useForm<BatchFormValues>({
    resolver: zodResolver(batchFormSchema),
    defaultValues: DEFAULT_VALUES,
  });
  const { errors } = form.formState;
  const urlsText = useWatch({ control: form.control, name: "urls" });
  const urlCount = parseUrls(urlsText).length;

  const start = useMutation({
    mutationFn: submitBatchScrape,
    onSuccess: (data) => {
      setJobId(data.job_id);
      setSubmittedCount(data.urls_count);
      queryClient.invalidateQueries({ queryKey: ["jobs"] });
      toast.success(data.message || "Batch scrape started");
    },
    onError: (err: Error) =>
      toast.error("Failed to start the batch", { description: errorMessage(err) }),
  });

  const job = useQuery({
    queryKey: ["job", jobId],
    queryFn: () => fetchJobStatus(jobId ?? ""),
    enabled: jobId !== null,
    refetchInterval: (query) => {
      const status = query.state.data?.status;
      return status && TERMINAL.includes(status) ? false : 1500;
    },
  });

  const cancel = useMutation({
    mutationFn: (id: string) => cancelJob(id),
    onSuccess: (_data, id) => {
      queryClient.invalidateQueries({ queryKey: ["job", id] });
      queryClient.invalidateQueries({ queryKey: jobResultsQueryKey(id) });
      queryClient.invalidateQueries({ queryKey: ["jobs"] });
      toast.success("Batch cancelled");
    },
    onError: (err: Error) =>
      toast.error("Failed to cancel", { description: errorMessage(err) }),
  });

  const onSubmit = form.handleSubmit((values) => {
    const request = toRequest(values);
    setLastRequest(request);
    start.mutate(request);
  });

  const status = job.data;
  const running = status ? !TERMINAL.includes(status.status) : start.isPending;
  const total = status?.max_pages ?? submittedCount;
  const done = status ? status.pages_crawled + status.errors : 0;
  const percent = total > 0 ? Math.min(100, Math.round((done / total) * 100)) : 0;

  return (
    <div className="flex flex-col gap-4 h-full">
      <div className="flex flex-wrap items-center gap-2">
        <Files className="h-5 w-5 text-primary" />
        <h1 className="text-lg font-semibold">Batch scrape</h1>
        <p className="text-sm text-muted-foreground">
          Scrape up to {MAX_URLS.toLocaleString()} URLs as one job and page
          through the results.
        </p>
      </div>

      <div className="grid grid-cols-1 lg:grid-cols-[minmax(360px,1fr)_3fr] gap-4 flex-1 min-h-0">
        <Card className="overflow-auto">
          <CardContent className="p-4">
            <form onSubmit={onSubmit} className="space-y-5" noValidate>
              <div className="space-y-1.5">
                <div className="flex items-baseline justify-between gap-2">
                  <Label htmlFor="urls" className="text-sm font-medium">
                    URLs
                  </Label>
                  <span className="text-xs text-muted-foreground">
                    {urlCount} / {MAX_URLS}
                  </span>
                </div>
                <p className="text-xs text-muted-foreground">One per line</p>
                <Textarea
                  id="urls"
                  rows={8}
                  spellCheck={false}
                  className="font-mono text-xs"
                  aria-invalid={errors.urls ? true : undefined}
                  {...form.register("urls")}
                />
                {errors.urls && (
                  <p className="text-xs text-destructive">{errors.urls.message}</p>
                )}
              </div>

              <div className="space-y-1.5">
                <Label className="text-sm font-medium">Formats</Label>
                <Controller
                  control={form.control}
                  name="formats"
                  render={({ field }) => (
                    <ToggleGroup
                      type="multiple"
                      variant="outline"
                      size="sm"
                      spacing={1}
                      className="flex-wrap"
                      value={field.value}
                      onValueChange={(v) => field.onChange(v)}
                      aria-invalid={errors.formats ? true : undefined}
                    >
                      {FORMATS.map((f) => (
                        <ToggleGroupItem
                          key={f.value}
                          value={f.value}
                          className="text-xs px-2.5"
                        >
                          {f.label}
                        </ToggleGroupItem>
                      ))}
                    </ToggleGroup>
                  )}
                />
                {errors.formats && (
                  <p className="text-xs text-destructive">{errors.formats.message}</p>
                )}
              </div>

              <div className="space-y-3 border-t pt-4">
                <Controller
                  control={form.control}
                  name="only_main_content"
                  render={({ field }) => (
                    <div className="flex items-center justify-between">
                      <div>
                        <Label htmlFor="only-main" className="text-sm font-medium cursor-pointer">
                          Main content only
                        </Label>
                        <p className="text-xs text-muted-foreground">
                          Exclude navigation, footer, sidebar
                        </p>
                      </div>
                      <Switch
                        id="only-main"
                        checked={field.value}
                        onCheckedChange={field.onChange}
                      />
                    </div>
                  )}
                />
                <Controller
                  control={form.control}
                  name="render_js"
                  render={({ field }) => (
                    <div className="flex items-center justify-between">
                      <div>
                        <Label htmlFor="render-js" className="text-sm font-medium cursor-pointer">
                          Render JavaScript
                        </Label>
                        <p className="text-xs text-muted-foreground">
                          Load every page in a headless browser
                        </p>
                      </div>
                      <Switch
                        id="render-js"
                        checked={field.value}
                        onCheckedChange={field.onChange}
                      />
                    </div>
                  )}
                />
                <div className="space-y-1.5">
                  <Label htmlFor="concurrency" className="text-sm font-medium">
                    Concurrency
                  </Label>
                  <p className="text-xs text-muted-foreground">
                    URLs scraped at once (1 to {MAX_CONCURRENCY})
                  </p>
                  <Input
                    id="concurrency"
                    type="number"
                    min={1}
                    max={MAX_CONCURRENCY}
                    aria-invalid={errors.concurrency ? true : undefined}
                    {...form.register("concurrency", { valueAsNumber: true })}
                  />
                  {errors.concurrency && (
                    <p className="text-xs text-destructive">
                      {errors.concurrency.message}
                    </p>
                  )}
                </div>
              </div>

              <div className="flex gap-2">
                <Button
                  type="submit"
                  className="flex-1"
                  disabled={start.isPending || (jobId !== null && running)}
                >
                  {start.isPending || (jobId !== null && running) ? (
                    <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                  ) : (
                    <Files className="mr-2 h-4 w-4" />
                  )}
                  Scrape {urlCount > 0 ? urlCount : ""} URL{urlCount === 1 ? "" : "s"}
                </Button>
                {jobId && running && (
                  <Button
                    type="button"
                    variant="outline"
                    onClick={() => cancel.mutate(jobId)}
                    disabled={cancel.isPending}
                  >
                    <Square className="mr-2 h-4 w-4" />
                    Cancel
                  </Button>
                )}
              </div>
            </form>
          </CardContent>
        </Card>

        <Card className="overflow-auto">
          <CardHeader className="pb-3">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <div className="flex items-center gap-3">
                <CardTitle className="text-base">Batch</CardTitle>
                {status && <StatusBadge status={status.status} />}
                {jobId && (
                  <span className="font-mono text-xs text-muted-foreground">
                    {jobId.slice(0, 8)}
                  </span>
                )}
              </div>
              {jobId && (
                <Button variant="outline" size="sm" asChild>
                  <Link href={`/dashboard/jobs/${jobId}`}>
                    Job details
                    <ArrowRight className="ml-1 h-3.5 w-3.5" />
                  </Link>
                </Button>
              )}
            </div>
          </CardHeader>
          <CardContent className="space-y-4">
            {!jobId && !start.isPending && (
              <div className="space-y-2">
                <p className="text-sm text-muted-foreground">
                  Start a batch to follow its progress and results here.
                  Equivalent API calls:
                </p>
                <CodeBlock code={curlExample(lastRequest)} lang="bash" />
              </div>
            )}

            {start.isPending && (
              <div className="flex items-center gap-2 text-sm text-muted-foreground">
                <Loader2 className="h-4 w-4 animate-spin" />
                Starting the batch…
              </div>
            )}

            {job.error && (
              <Alert variant="destructive">
                <AlertDescription>{errorMessage(job.error)}</AlertDescription>
              </Alert>
            )}

            {status?.error_message && (
              <Alert variant="destructive">
                <XCircle className="h-4 w-4" />
                <AlertDescription>{status.error_message}</AlertDescription>
              </Alert>
            )}

            {status && (
              <div className="space-y-2">
                <div className="flex items-center justify-between text-xs text-muted-foreground">
                  <span>
                    <span className="font-medium text-foreground">{done}</span> of{" "}
                    {total} URLs done
                  </span>
                  <span className="flex items-center gap-3">
                    <span>
                      <span className="font-medium text-emerald-500">
                        {status.pages_crawled}
                      </span>{" "}
                      scraped
                    </span>
                    <span>
                      <span
                        className={
                          status.errors > 0
                            ? "font-medium text-destructive"
                            : "font-medium text-foreground"
                        }
                      >
                        {status.errors}
                      </span>{" "}
                      failed
                    </span>
                    <span>{percent}%</span>
                  </span>
                </div>
                <Progress value={percent} className="h-1.5" />
              </div>
            )}

            {jobId && <JobResults jobId={jobId} />}
          </CardContent>
        </Card>
      </div>
    </div>
  );
}
