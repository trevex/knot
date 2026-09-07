import React from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { CommentComposer } from "./CommentComposer";

const CAROL = {
  user_id: "11111111-1111-1111-1111-111111111111",
  display_name: "Carol Danvers",
  email: "carol@x.test",
  role: "editor",
};

vi.mock("../workspace/workspace.api", () => ({
  workspaceApi: { listMembers: vi.fn(() => Promise.resolve({ ok: [CAROL] })) },
}));

afterEach(cleanup);

function renderComposer() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const onSubmit = vi.fn();
  render(
    <QueryClientProvider client={qc}>
      <CommentComposer
        onSubmit={onSubmit}
        data-testid-input="composer-input"
        data-testid-submit="composer-submit"
      />
    </QueryClientProvider>,
  );
  return { onSubmit };
}

describe("CommentComposer mentions", () => {
  it("submits the picked member's id, not just the typed text", async () => {
    const { onSubmit } = renderComposer();
    const textarea = screen.getByTestId<HTMLTextAreaElement>("composer-input");

    // Type "@Car" and put the caret at the end so the hook parses a mention.
    fireEvent.change(textarea, { target: { value: "@Car" } });
    textarea.selectionStart = 4;
    fireEvent.keyUp(textarea);

    const row = await screen.findByTestId("mention-item");
    fireEvent.mouseDown(row);

    fireEvent.click(screen.getByTestId("composer-submit"));
    expect(onSubmit).toHaveBeenCalledWith("@Carol Danvers", [CAROL.user_id]);
  });

  it("submits an empty id list when nobody was picked", () => {
    const { onSubmit } = renderComposer();
    const textarea = screen.getByTestId("composer-input");
    fireEvent.change(textarea, { target: { value: "no mentions here" } });
    fireEvent.click(screen.getByTestId("composer-submit"));
    expect(onSubmit).toHaveBeenCalledWith("no mentions here", []);
  });

  it("drops a picked id whose name was backspaced back out of the body", async () => {
    const { onSubmit } = renderComposer();
    const textarea = screen.getByTestId<HTMLTextAreaElement>("composer-input");

    fireEvent.change(textarea, { target: { value: "@Car" } });
    textarea.selectionStart = 4;
    fireEvent.keyUp(textarea);
    const row = await screen.findByTestId("mention-item");
    fireEvent.mouseDown(row);
    // Picking inserts "@Carol Danvers " — now remove her name entirely,
    // as if the author backspaced it out, but left other text behind.
    fireEvent.change(textarea, { target: { value: "never mind" } });

    fireEvent.click(screen.getByTestId("composer-submit"));
    expect(onSubmit).toHaveBeenCalledWith("never mind", []);
  });

  it("keeps a picked id whose name is still present in the body", async () => {
    const { onSubmit } = renderComposer();
    const textarea = screen.getByTestId<HTMLTextAreaElement>("composer-input");

    fireEvent.change(textarea, { target: { value: "@Car" } });
    textarea.selectionStart = 4;
    fireEvent.keyUp(textarea);
    const row = await screen.findByTestId("mention-item");
    fireEvent.mouseDown(row);

    fireEvent.click(screen.getByTestId("composer-submit"));
    expect(onSubmit).toHaveBeenCalledWith("@Carol Danvers", [CAROL.user_id]);
  });
});
