import type { CommitDraftContext, CommitMessageDraft, WorkflowRun } from "./settings";

export async function automaticallyPush(services: {
  prepare(): Promise<CommitDraftContext>;
  draft(fingerprint: string): Promise<CommitMessageDraft>;
  start(context: CommitDraftContext, subject: string, body: string): Promise<WorkflowRun>;
}, progress: (message: string) => void): Promise<WorkflowRun> {
  progress("Preparing changes…");
  const context = await services.prepare();
  let subject = "", body = "";
  if (context.mode !== "pushOnly") {
    progress("Drafting message and description…");
    const draft = await services.draft(context.snapshotFingerprint);
    if (draft.snapshotFingerprint !== context.snapshotFingerprint) throw new Error("Changes changed while drafting; retry Push");
    subject = draft.subject.trim(); body = draft.body.trim();
    if (!subject) throw new Error("Could not generate a commit message");
  }
  progress("Starting commit and push…");
  return services.start(context, subject, body);
}
