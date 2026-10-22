import type { CommitDraftContext, CommitMessageDraft, WorkflowRun } from "./settings";

export async function automaticallyPush(services: {
  prepare(): Promise<CommitDraftContext>;
  draft(fingerprint: string): Promise<CommitMessageDraft>;
  start(context: CommitDraftContext, subject: string, body: string): Promise<WorkflowRun>;
}, progress: (message: string) => void): Promise<WorkflowRun> {
  for (let attempt = 0; attempt < 2; attempt += 1) {
    progress(attempt === 0 ? "Preparing changes…" : "Changes updated. Refreshing draft…");
    const context = await services.prepare();
    let subject = "", body = "";
    if (context.mode !== "pushOnly") {
      progress("Drafting message and description…");
      try {
        const draft = await services.draft(context.snapshotFingerprint);
        if (draft.snapshotFingerprint !== context.snapshotFingerprint) throw new Error("Repository changes shifted while drafting.");
        subject = draft.subject.trim(); body = draft.body.trim();
        if (!subject) throw new Error("Could not generate a commit message");
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        if (!message.startsWith("Repository changes shifted")) throw error;
        if (attempt === 0) continue;
        throw new Error("Files keep changing while preparing Push. Wait for edits to finish, then click Push again.");
      }
    }
    progress("Starting commit and push…");
    // Never retry start: an error may arrive after a run was admitted.
    return services.start(context, subject, body);
  }
  throw new Error("Could not prepare Push. Click Push to retry.");
}
