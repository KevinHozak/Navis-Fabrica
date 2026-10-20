import type { CommitDraftContext, CommitMessageDraft } from "./settings";

export type CommitReviewState = {
  paths: string[] | null;
  context: CommitDraftContext | null;
  phase: "files" | "preparing" | "drafting" | "ready";
  subject: string;
  body: string;
  editedSubject: boolean;
  editedBody: boolean;
  frozen: boolean;
  error: string;
};
export const initialCommitReview: CommitReviewState = {
  paths: null, context: null, phase: "files", subject: "", body: "",
  editedSubject: false, editedBody: false, frozen: false, error: "",
};
type Action =
  | { type: "reset" }
  | { type: "paths"; paths: string[] }
  | { type: "prepared"; context: CommitDraftContext }
  | { type: "draft"; draft: CommitMessageDraft }
  | { type: "subject" | "body"; value: string }
  | { type: "freeze" | "thaw" }
  | { type: "error"; error: string };

export function commitReviewReducer(state: CommitReviewState, action: Action): CommitReviewState {
  if (action.type === "reset") return { ...initialCommitReview };
  if (action.type === "thaw") return { ...state, frozen: false };
  if (state.frozen) return state;
  switch (action.type) {
    case "paths": return { ...state, paths: action.paths, phase: "preparing" };
    case "prepared": return { ...state, context: action.context, paths: action.context.paths,
      phase: action.context.mode === "pushOnly" ? "ready" : "drafting" };
    case "draft": return { ...state, phase: "ready",
      subject: state.editedSubject ? state.subject : action.draft.subject,
      body: state.editedBody ? state.body : action.draft.body };
    case "subject": return { ...state, subject: action.value, editedSubject: true };
    case "body": return { ...state, body: action.value, editedBody: true };
    case "freeze": return { ...state, frozen: true, phase: "ready" };
    case "error": return { ...state, error: action.error, phase: "ready" };
  }
}

// File discovery is a separate, cheap IPC call. Never await full preparation
// or inference before delivering its result. Disposed dialogs ignore late work.
export async function loadCommitReview(services: {
  paths(): Promise<string[]>;
  prepare(): Promise<CommitDraftContext>;
  draft(fingerprint: string): Promise<CommitMessageDraft>;
}, dispatch: (action: Action) => void, active: () => boolean): Promise<void> {
  try {
    const paths = await services.paths();
    if (!active()) return;
    dispatch({ type: "paths", paths });
    const context = await services.prepare();
    if (!active()) return;
    dispatch({ type: "prepared", context });
    if (context.mode === "pushOnly") return;
    const draft = await services.draft(context.snapshotFingerprint);
    if (active()) dispatch({ type: "draft", draft });
  } catch (reason) {
    if (active()) dispatch({ type: "error", error: reason instanceof Error ? reason.message : String(reason) });
  }
}
