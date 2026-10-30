// Brings one pull request to the state its triage result and labels call for.
// Each run reads the pull request and its timeline as they are now and takes
// only steps that are safe to repeat, so a run that stops halfway is finished
// by the next one; the scheduled run guarantees there is a next one.
//
// Triage fails: draft, no `triage:passed`, and a comment listing what is
// missing. A pull request the gate drafts carries `triage:held` while drafted.
// Triage passes: `triage:passed`, then a "ready for review" event after that
// label, which is what starts Devin's rule. A draft the author made stays a
// draft; their own "ready" click starts Devin.
// `slop`: a "ready for review" event after the label starts Devin's Slop
// rule; once its status lands, the closing comment and close.
const fs = require('fs');
const problems = require('./triage.js');

const MARKER = '<!-- og-triage -->';
const SLOP_MARKER = '<!-- og-slop -->';
const PASSED = 'triage:passed';
const HELD = 'triage:held';
const SLOP = 'slop';
const DEVIN = 'devin-ai-integration[bot]';
const SLOP_CHECK = 'Devin Review / Slop';

module.exports = async function reconcile({ github, drafts, context, core, number }) {
  const repo = context.repo;
  const issue = { ...repo, issue_number: number };
  const pr = (await github.rest.pulls.get({ ...repo, pull_number: number })).data;
  if (pr.state !== 'open') return;
  const labels = new Set(pr.labels.map((label) => label.name));
  let draft = pr.draft;
  let readied = false;
  const added = new Set();

  const addLabel = async (name) => {
    if (labels.has(name)) return;
    await github.rest.issues.addLabels({ ...issue, labels: [name] });
    labels.add(name);
    added.add(name);
  };
  const removeLabel = async (name) => {
    if (!labels.has(name)) return;
    try {
      await github.rest.issues.removeLabel({ ...issue, name });
    } catch (error) {
      if (error.status !== 404) throw error;
    }
    labels.delete(name);
  };
  const toDraft = async () => {
    if (draft) return;
    await drafts.graphql(`mutation($id: ID!) { convertPullRequestToDraft(input: { pullRequestId: $id }) { clientMutationId } }`, { id: pr.node_id });
    draft = true;
  };
  const toReady = async () => {
    if (!draft) return;
    await drafts.graphql(`mutation($id: ID!) { markPullRequestReadyForReview(input: { pullRequestId: $id }) { clientMutationId } }`, { id: pr.node_id });
    draft = false;
    readied = true;
  };

  // Timeline events come oldest first, so positions order them exactly where
  // timestamps can tie within a second.
  const timeline = await github.paginate(github.rest.issues.listEventsForTimeline, { ...issue, per_page: 100 });
  const last = (match) => timeline.findLastIndex(match);
  // The timeline was read before this run's own label, which no earlier
  // ready event can follow.
  const readyAfter = (name) =>
    readied ||
    (!added.has(name) &&
      last((e) => e.event === 'ready_for_review') > last((e) => e.event === 'labeled' && e.label?.name === name));

  // A "ready for review" event after `name` was added, through draft if the
  // pull request is ready already. `triage:held` marks the pull request as the
  // gate's while it is drafted, so a rerun finishes the trip.
  const readyEventAfter = async (name) => {
    if (!draft && readyAfter(name)) {
      await removeLabel(HELD);
      return;
    }
    await addLabel(HELD);
    await toDraft();
    await toReady();
    await removeLabel(HELD);
  };

  const comments = await github.paginate(github.rest.issues.listComments, { ...issue, per_page: 100 });
  const ours = (marker) => comments.find((c) => c.user.type === 'Bot' && c.body.startsWith(marker));
  const guide = `https://github.com/${repo.owner}/${repo.repo}/blob/main/CONTRIBUTING.md`;

  if (labels.has(SLOP)) {
    const statuses = await github.paginate(github.rest.repos.listCommitStatusesForRef, { ...repo, ref: pr.head.sha, per_page: 100 });
    const done = statuses.find((s) => s.context === SLOP_CHECK && s.creator?.login === DEVIN && s.state !== 'pending');
    if (!done) {
      await readyEventAfter(SLOP);
      return;
    }
    if (!ours(SLOP_MARKER)) {
      const findings = typeof done.target_url === 'string' && done.target_url.startsWith('https://app.devin.ai/')
        ? `[${SLOP_CHECK}](${done.target_url})`
        : `see the **${SLOP_CHECK}** check on this pull request`;
      const template = fs.readFileSync(`${__dirname}/../slop.md`, 'utf8');
      const body = `${SLOP_MARKER}\n${template.replace('{{findings}}', findings).replace('{{guide}}', guide)}`;
      await github.rest.issues.createComment({ ...issue, body });
    }
    await github.rest.pulls.update({ ...repo, pull_number: number, state: 'closed' });
    return;
  }

  const found = problems({ title: pr.title, body: pr.body, labels: [...labels] });
  const mine = ours(MARKER);

  if (found.length) {
    const message = `${MARKER}\nTriage found a few things to fix before review:\n\n${found.map((p) => `- ${p}`).join('\n')}\n\nThe templates and rules are in [CONTRIBUTING.md](${guide}). This pull request stays a draft until they pass; it reruns when you push or edit the description.`;
    if (!mine) await github.rest.issues.createComment({ ...issue, body: message });
    else if (mine.body !== message) await github.rest.issues.updateComment({ ...repo, comment_id: mine.id, body: message });
    await removeLabel(PASSED);
    if (!draft) {
      await addLabel(HELD);
      await toDraft();
    }
    return;
  }

  if (mine) await github.rest.issues.deleteComment({ ...repo, comment_id: mine.id });
  // The label goes on before the ready event, so that event carries it.
  await addLabel(PASSED);
  if (draft && !labels.has(HELD)) {
    core.info(`#${number}: triage passed; the author's draft stays a draft`);
    return;
  }
  await readyEventAfter(PASSED);
  core.info(`#${number}: triage passed, ready`);
};
