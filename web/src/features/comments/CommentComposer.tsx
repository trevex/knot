import type React from "react";
import { useState } from "react";
import { useMentionPicker } from "./MentionPicker";

interface Props {
  placeholder?: string;
  submitLabel?: string;
  isPending?: boolean;
  onSubmit: (body: string, mentions: string[]) => void;
  "data-testid-input"?: string;
  "data-testid-submit"?: string;
}

export function CommentComposer({
  placeholder = "Write a comment…",
  submitLabel = "Submit",
  isPending = false,
  onSubmit,
  "data-testid-input": testidInput,
  "data-testid-submit": testidSubmit,
}: Props) {
  const [body, setBody] = useState("");
  const [mentions, setMentions] = useState<string[]>([]);
  const { textareaProps, picker } = useMentionPicker(body, setBody, (id) =>
    setMentions((prev) => (prev.includes(id) ? prev : [...prev, id])),
  );

  function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    const trimmed = body.trim();
    if (!trimmed) return;
    onSubmit(trimmed, mentions);
    setBody("");
    setMentions([]);
  }

  return (
    <form onSubmit={handleSubmit} className="p-3">
      <div className="relative">
        <textarea
          data-testid={testidInput}
          value={body}
          onChange={(e) => setBody(e.target.value)}
          placeholder={placeholder}
          rows={3}
          className="w-full resize-y px-2 py-1.5 text-sm rounded border border-border bg-bg text-fg placeholder:text-fg-muted focus:outline-none focus:ring-2 focus:ring-accent"
          {...textareaProps}
        />
        {picker}
      </div>
      <button
        type="submit"
        data-testid={testidSubmit}
        disabled={isPending || body.trim().length === 0}
        className="mt-2 inline-flex items-center h-8 px-3 rounded bg-accent text-accent-fg text-[13px] font-medium hover:opacity-90 transition-opacity disabled:opacity-40 disabled:cursor-not-allowed"
      >
        {isPending ? "Submitting…" : submitLabel}
      </button>
    </form>
  );
}
