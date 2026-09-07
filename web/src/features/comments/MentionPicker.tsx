/**
 * MentionPicker — detects "@" in a textarea and shows a member dropdown.
 *
 * Usage: wrap the textarea with <div style={{position:"relative"}}> and
 * spread the returned `textareaProps` onto the <textarea>. The returned
 * `picker` element should be rendered inside that same div.
 */

import type React from "react";
import { useQuery } from "@tanstack/react-query";
import { useState } from "react";

import { workspaceApi } from "../workspace/workspace.api";

interface MentionState {
  query: string;
  atOffset: number;
}

function parseMention(value: string, cursorPos: number): MentionState | null {
  let i = cursorPos - 1;
  while (i >= 0 && value[i] !== "@" && !/\s/.test(value[i] ?? "")) {
    i--;
  }
  if (i < 0 || value[i] !== "@") return null;
  const query = value.slice(i + 1, cursorPos);
  // Don't activate if the query contains spaces (already typed past a word boundary)
  if (/\s/.test(query)) return null;
  return { query, atOffset: i };
}

export function useMentionPicker(
  value: string,
  onChange: (v: string) => void,
  onPickUser?: (userId: string) => void,
) {
  const [cursor, setCursor] = useState(0);
  const [highlightIndex, setHighlightIndex] = useState(0);

  const members = useQuery({
    queryKey: ["members"],
    queryFn: () => workspaceApi.listMembers(),
    staleTime: 60_000,
  });

  const memberList = members.data && "ok" in members.data ? members.data.ok : [];
  const mention = parseMention(value, cursor);

  const filtered = mention
    ? memberList.filter((m) =>
        m.display_name.toLowerCase().includes(mention.query.toLowerCase()) ||
        m.email.toLowerCase().includes(mention.query.toLowerCase()),
      )
    : [];

  function pick(member: { user_id: string; display_name: string }) {
    if (!mention) return;
    const before = value.slice(0, mention.atOffset);
    const after = value.slice(cursor);
    onChange(before + `@${member.display_name} ` + after);
    onPickUser?.(member.user_id);
    setHighlightIndex(0);
  }

  const isOpen = mention !== null && filtered.length > 0;

  const textareaProps = {
    onSelect: (e: React.SyntheticEvent<HTMLTextAreaElement>) => {
      setCursor(e.currentTarget.selectionStart);
    },
    onKeyUp: (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
      setCursor(e.currentTarget.selectionStart);
    },
    onKeyDown: (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
      if (!isOpen) return;
      if (e.key === "ArrowDown") {
        e.preventDefault();
        setHighlightIndex((i) => Math.min(i + 1, filtered.length - 1));
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setHighlightIndex((i) => Math.max(i - 1, 0));
      } else if (e.key === "Enter") {
        const picked = filtered[highlightIndex];
        if (picked) {
          e.preventDefault();
          pick(picked);
        }
      } else if (e.key === "Escape") {
        e.preventDefault();
        // Clear cursor pos so mention state resets
        setCursor(0);
      }
    },
  };

  const picker = isOpen ? (
    <ul
      role="listbox"
      className="absolute top-full left-0 right-0 z-50 m-0 p-0 list-none bg-surface border border-border rounded-md shadow-lg max-h-[200px] overflow-y-auto"
    >
      {filtered.map((m, i) => (
        <li
          key={m.user_id}
          role="option"
          aria-selected={i === highlightIndex}
          data-testid="mention-item"
          onMouseDown={(e) => { e.preventDefault(); pick(m); }}
          className={`px-3 py-2 cursor-pointer text-sm ${
            i === highlightIndex ? "bg-muted text-fg" : "text-fg hover:bg-muted/60"
          }`}
        >
          <strong className="font-semibold">{m.display_name}</strong>{" "}
          <span className="text-fg-muted text-[12px]">{m.email}</span>
        </li>
      ))}
    </ul>
  ) : null;

  return { textareaProps, picker };
}
