"use client";

import { useState } from "react";
import { Controller, useForm } from "react-hook-form";
import { zodResolver } from "@hookform/resolvers/zod";
import { z } from "zod";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";
import {
  AlertTriangle,
  CheckCircle2,
  Copy,
  ExternalLink,
  Loader2,
  Sparkles,
  Square,
  XCircle,
} from "lucide-react";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { HighlightedJson } from "@/components/highlighted-json";
import { CodeBlock } from "@/app/dashboard/playground/result-panel";
import { deleteJob, fetchExtract, submitExtract } from "@/lib/api";
import type { ExtractRequest, ExtractStatus } from "@/lib/api-types";

const MAX_URLS = 50;
const TERMINAL = ["completed", "failed", "cancelled"];

function parseUrls(text: string): string[] {
  return text
    .split(/[\n,]/)
    .map((u) => u.trim())
    .filter(Boolean);
}

const extractFormSchema = z
  .object({
    urls: z.string().trim().min(1, "Enter at least one URL"),
    prompt: z.string(),
    schema: z.string(),
    render_js: z.boolean(),
    only_main_content: z.boolean(),
  })
  .superRefine((values, ctx) => {
    const urls = parseUrls(values.urls);
    const invalid = urls.find((u) => !/^https?:\/\/[^/*]+/i.test(u));
    if (invalid) {
      ctx.addIssue({
        code: "custom",
        path: ["urls"],
        message: `Not an http(s) URL with a fixed host: ${invalid}`,
      });
    }
    if (urls.length > MAX_URLS) {
      ctx.addIssue({
        code: "custom",
        path: ["urls"],
        message: `At most ${MAX_URLS} URLs`,
      });
    }
    if (!values.prompt.trim() && !values.schema.trim()) {
      ctx.addIssue({
        code: "custom",
        path: ["prompt"],
        message: "Describe what to extract, or provide a schema",
      });
    }
    if (values.schema.trim()) {
      try {
        const parsed: unknown = JSON.parse(values.schema);
        if (typeof parsed !== "object" || parsed === null) {
          throw new Error("not an object");
        }
      } catch {
        ctx.addIssue({
          code: "custom",
          path: ["schema"],
          message: "Must be a JSON Schema object or a list of field definitions",
        });
      }
    }
  });

type ExtractFormValues = z.infer<typeof extractFormSchema>;

const DEFAULT_VALUES: ExtractFormValues = {
  urls: "https://scrapix.meilisearch.dev",
  prompt: "Summarize what this product does and list its main features.",
  schema: "",
  render_js: false,
  only_main_content: true,
};

function toRequest(values: ExtractFormValues): ExtractRequest {
  const schema = values.schema.trim()
    ? (JSON.parse(values.schema) as ExtractRequest["schema"])
    : undefined;
  return {
    urls: parseUrls(values.urls),
    prompt: values.prompt.trim() || undefined,
    schema,
    render_js: values.render_js,
    only_main_content: values.only_main_content,
  };
}

function curlExample(request: ExtractRequest): string {
  return `curl -X POST https://scrapix.meilisearch.dev/extract \\
  -H "Content-Type: application/json" \\
  -H "Authorization: Bearer YOUR_API_KEY" \\
  -d '${JSON.stringify(request)}'

# then poll
curl https://scrapix.meilisearch.dev/extract/JOB_ID \\
  -H "Authorization: Bearer YOUR_API_KEY"`;
}

function StatusBadge({ status }: { status: string }) {
  const variant =
    status === "failed"
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

function SourcesTable({ sources }: { sources: ExtractStatus["sources"] }) {
  if (sources.length === 0) return null;
  return (
    <Table>
      <TableHeader>
        <TableRow>
          <TableHead className="w-10" />
          <TableHead>URL</TableHead>
          <TableHead>From glob</TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {sources.map((source) => (
          <TableRow key={source.url}>
            <TableCell>
              {source.success === true && !source.error ? (
                <CheckCircle2 className="h-4 w-4 text-emerald-500" />
              ) : source.success === null ? (
                <Loader2 className="h-4 w-4 animate-spin text-muted-foreground" />
              ) : (
                <XCircle className="h-4 w-4 text-destructive" />
              )}
            </TableCell>
            <TableCell className="max-w-[420px]">
              <a
                href={source.url}
                target="_blank"
                rel="noreferrer"
                className="inline-flex items-center gap-1 font-mono text-xs hover:underline"
              >
                <span className="truncate">{source.url}</span>
                <ExternalLink className="h-3 w-3 shrink-0" />
              </a>
              {source.error && (
                <p className="text-xs text-destructive mt-0.5">{source.error}</p>
              )}
            </TableCell>
            <TableCell className="font-mono text-xs text-muted-foreground">
              {source.from_glob ?? "—"}
            </TableCell>
          </TableRow>
        ))}
      </TableBody>
    </Table>
  );
}

export default function ExtractPage() {
  const queryClient = useQueryClient();
  const [jobId, setJobId] = useState<string | null>(null);
  const [lastRequest, setLastRequest] = useState<ExtractRequest>(
    toRequest(DEFAULT_VALUES),
  );

  const form = useForm<ExtractFormValues>({
    resolver: zodResolver(extractFormSchema),
    defaultValues: DEFAULT_VALUES,
  });
  const { errors } = form.formState;

  const start = useMutation({
    mutationFn: submitExtract,
    onSuccess: (data) => {
      setJobId(data.job_id);
      queryClient.invalidateQueries({ queryKey: ["jobs"] });
      toast.success("Extraction started");
    },
    onError: (err: Error) => toast.error(err.message || "Failed to start extraction"),
  });

  const extraction = useQuery({
    queryKey: ["extract", jobId],
    queryFn: () => fetchExtract(jobId ?? ""),
    enabled: jobId !== null,
    refetchInterval: (query) => {
      const status = query.state.data?.status;
      return status && TERMINAL.includes(status) ? false : 1500;
    },
  });

  const cancel = useMutation({
    mutationFn: (id: string) => deleteJob(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["extract", jobId] });
      queryClient.invalidateQueries({ queryKey: ["jobs"] });
      toast.success("Extraction cancelled");
    },
  });

  const onSubmit = form.handleSubmit((values) => {
    const request = toRequest(values);
    setLastRequest(request);
    start.mutate(request);
  });

  const result = extraction.data;
  const running = result ? !TERMINAL.includes(result.status) : start.isPending;
  const dataJson =
    result?.data !== undefined && result?.data !== null
      ? JSON.stringify(result.data, null, 2)
      : null;

  return (
    <div className="flex flex-col gap-4 h-full">
      <div className="flex flex-wrap items-center gap-2">
        <Sparkles className="h-5 w-5 text-primary" />
        <h1 className="text-lg font-semibold">Extract</h1>
        <p className="text-sm text-muted-foreground">
          Structured data from one or many pages. Globs like{" "}
          <code className="font-mono text-xs">https://example.com/blog/*</code> are
          resolved first.
        </p>
      </div>

      <div className="grid grid-cols-1 lg:grid-cols-[minmax(360px,1fr)_3fr] gap-4 flex-1 min-h-0">
        <Card className="overflow-auto">
          <CardContent className="p-4">
            <form onSubmit={onSubmit} className="space-y-5" noValidate>
              <div className="space-y-1.5">
                <Label htmlFor="urls" className="text-sm font-medium">
                  URLs
                </Label>
                <p className="text-xs text-muted-foreground">
                  One per line, {MAX_URLS} max after glob resolution
                </p>
                <Textarea
                  id="urls"
                  rows={4}
                  className="font-mono text-xs"
                  aria-invalid={errors.urls ? true : undefined}
                  {...form.register("urls")}
                />
                {errors.urls && (
                  <p className="text-xs text-destructive">{errors.urls.message}</p>
                )}
              </div>

              <div className="space-y-1.5">
                <Label htmlFor="prompt" className="text-sm font-medium">
                  Prompt
                </Label>
                <Textarea
                  id="prompt"
                  rows={4}
                  className="text-sm"
                  placeholder="What should be extracted?"
                  aria-invalid={errors.prompt ? true : undefined}
                  {...form.register("prompt")}
                />
                {errors.prompt && (
                  <p className="text-xs text-destructive">{errors.prompt.message}</p>
                )}
              </div>

              <div className="space-y-1.5">
                <Label htmlFor="schema" className="text-sm font-medium">
                  Schema <span className="text-muted-foreground font-normal">(optional)</span>
                </Label>
                <p className="text-xs text-muted-foreground">
                  A JSON Schema object, or a list of fields
                </p>
                <Textarea
                  id="schema"
                  rows={6}
                  className="font-mono text-xs"
                  placeholder={'{\n  "type": "object",\n  "properties": {\n    "features": { "type": "array", "items": { "type": "string" } }\n  }\n}'}
                  aria-invalid={errors.schema ? true : undefined}
                  {...form.register("schema")}
                />
                {errors.schema && (
                  <p className="text-xs text-destructive">{errors.schema.message}</p>
                )}
              </div>

              <div className="space-y-3 border-t pt-4">
                <Controller
                  control={form.control}
                  name="only_main_content"
                  render={({ field }) => (
                    <div className="flex items-center justify-between">
                      <Label htmlFor="only-main" className="text-sm font-medium cursor-pointer">
                        Main content only
                      </Label>
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
                      <Label htmlFor="render-js" className="text-sm font-medium cursor-pointer">
                        Render JavaScript
                      </Label>
                      <Switch
                        id="render-js"
                        checked={field.value}
                        onCheckedChange={field.onChange}
                      />
                    </div>
                  )}
                />
              </div>

              <div className="flex gap-2">
                <Button type="submit" className="flex-1" disabled={start.isPending || running}>
                  {start.isPending || running ? (
                    <Loader2 className="mr-2 h-4 w-4 animate-spin" />
                  ) : (
                    <Sparkles className="mr-2 h-4 w-4" />
                  )}
                  Extract
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
                <CardTitle className="text-base">Result</CardTitle>
                {result && <StatusBadge status={result.status} />}
                {result && (
                  <Badge variant="outline" className="font-mono text-xs font-normal">
                    {result.sources.length} source{result.sources.length === 1 ? "" : "s"}
                  </Badge>
                )}
              </div>
              {dataJson && (
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => {
                    navigator.clipboard.writeText(dataJson);
                    toast.success("Copied JSON");
                  }}
                >
                  <Copy className="mr-1.5 h-3 w-3" />
                  Copy JSON
                </Button>
              )}
            </div>
          </CardHeader>
          <CardContent className="space-y-4">
            {!jobId && !start.isPending && (
              <div className="space-y-2">
                <p className="text-sm text-muted-foreground">
                  Run an extraction to see its result here. Equivalent API call:
                </p>
                <CodeBlock code={curlExample(lastRequest)} lang="bash" />
              </div>
            )}

            {extraction.error && (
              <Alert variant="destructive">
                <AlertDescription>{extraction.error.message}</AlertDescription>
              </Alert>
            )}

            {result?.error && (
              <Alert variant="destructive">
                <XCircle className="h-4 w-4" />
                <AlertDescription>{result.error}</AlertDescription>
              </Alert>
            )}

            {result?.warning && (
              <Alert>
                <AlertTriangle className="h-4 w-4" />
                <AlertDescription>{result.warning}</AlertDescription>
              </Alert>
            )}

            {(start.isPending || (result && running && !dataJson)) && (
              <div className="flex items-center gap-2 text-sm text-muted-foreground">
                <Loader2 className="h-4 w-4 animate-spin" />
                {result && result.sources.length > 0
                  ? "Fetching pages and extracting..."
                  : "Resolving URLs..."}
              </div>
            )}

            {dataJson && (
              <div className="rounded-md border overflow-auto max-h-[480px]">
                <HighlightedJson code={dataJson} />
              </div>
            )}

            {result && <SourcesTable sources={result.sources} />}
          </CardContent>
        </Card>
      </div>
    </div>
  );
}
