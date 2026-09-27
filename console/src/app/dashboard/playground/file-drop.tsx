"use client";

import { useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { FileUp, FileText, Loader2, Play, X } from "lucide-react";
import { toast } from "sonner";

/** Default server cap (`DOCUMENT_MAX_SIZE_MB`); the server has the final say. */
const MAX_BYTES = 50 * 1024 * 1024;

const ACCEPT = [
  ".pdf",
  ".doc",
  ".docx",
  ".docm",
  ".ppt",
  ".pptx",
  ".pptm",
  ".pps",
  ".ppsx",
  ".xls",
  ".xlsx",
  ".xlsm",
  ".xlsb",
  ".odt",
  ".ods",
  ".odp",
  ".rtf",
  ".epub",
  ".csv",
  "image/png",
  "image/jpeg",
  "image/gif",
  "image/webp",
  "image/tiff",
].join(",");

export function formatBytes(bytes: number): string {
  // `toFixed(1)` then drop a trailing ".0": 1.7 KB, 50 MB.
  const fmt = (n: number) => n.toFixed(1).replace(/\.0$/, "");
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${fmt(bytes / 1024)} KB`;
  return `${fmt(bytes / (1024 * 1024))} MB`;
}

interface FileDropProps {
  file: File | null;
  onFileChange: (file: File | null) => void;
  onSubmit: () => void;
  loading: boolean;
}

export function FileDrop({ file, onFileChange, onSubmit, loading }: FileDropProps) {
  const inputRef = useRef<HTMLInputElement>(null);
  const [dragging, setDragging] = useState(false);

  const pick = (files: FileList | null) => {
    const next = files?.[0];
    if (!next) return;
    if (next.size > MAX_BYTES) {
      toast.error(`File too large (max ${formatBytes(MAX_BYTES)})`);
      return;
    }
    onFileChange(next);
  };

  return (
    <div className="flex flex-col gap-2">
      <div className="flex items-stretch gap-2 rounded-lg border bg-card p-2">
        <button
          type="button"
          onClick={() => inputRef.current?.click()}
          onDragOver={(e) => {
            e.preventDefault();
            setDragging(true);
          }}
          onDragLeave={() => setDragging(false)}
          onDrop={(e) => {
            e.preventDefault();
            setDragging(false);
            pick(e.dataTransfer.files);
          }}
          className={cn(
            "flex flex-1 items-center gap-3 rounded-md border border-dashed px-3 py-2 text-left text-sm transition-colors",
            dragging
              ? "border-primary bg-primary/5"
              : "border-muted-foreground/25 hover:border-muted-foreground/50",
          )}
        >
          {file ? (
            <>
              <FileText className="h-4 w-4 shrink-0 text-primary" />
              <span className="truncate font-mono">{file.name}</span>
              <span className="shrink-0 text-xs text-muted-foreground">
                {formatBytes(file.size)}
              </span>
            </>
          ) : (
            <>
              <FileUp className="h-4 w-4 shrink-0 text-muted-foreground" />
              <span className="text-muted-foreground">
                Drop a document here, or click to choose one
              </span>
            </>
          )}
        </button>
        <input
          ref={inputRef}
          type="file"
          accept={ACCEPT}
          className="hidden"
          onChange={(e) => {
            pick(e.target.files);
            // Choosing the same file again still fires `change`.
            e.target.value = "";
          }}
        />

        {file && (
          <Button
            variant="ghost"
            size="icon"
            className="shrink-0 self-center"
            onClick={() => onFileChange(null)}
            disabled={loading}
            aria-label="Remove file"
          >
            <X className="h-4 w-4" />
          </Button>
        )}

        <Button
          onClick={onSubmit}
          disabled={loading || !file}
          className="shrink-0 gap-2 self-center"
        >
          {loading ? (
            <Loader2 className="h-4 w-4 animate-spin" />
          ) : (
            <Play className="h-4 w-4" />
          )}
          Parse
        </Button>
      </div>

      <p className="text-xs text-muted-foreground px-1">
        Convert a PDF, Word, Excel, PowerPoint, OpenDocument, RTF, EPUB or CSV file to
        Markdown. Up to {formatBytes(MAX_BYTES)}.
      </p>
    </div>
  );
}
