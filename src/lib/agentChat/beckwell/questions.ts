// Adapted from Beckwell (Cyberneura), MIT, via Astragal. See docs/agent-chat.md.
// Interprets the input of AskUserQuestion (the tool Claude Code uses to ask the user by presenting choices) and builds the answers
import type { Question, QuestionDraft } from './types';

const QUESTION_KINDS = new Set(['choice', 'text', 'number']);

function optionalNumber(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

// Extracts the questions from the tool input. null if it cannot be interpreted as questions (treated as an ordinary permission prompt)
export function parseQuestions(input: Record<string, unknown>): Question[] | null {
  const raw = input.questions;
  if (!Array.isArray(raw) || raw.length === 0) return null;
  const questions: Question[] = [];
  for (const entry of raw) {
    if (!entry || typeof entry !== 'object' || typeof entry.question !== 'string' || !entry.question) return null;
    const kind = QUESTION_KINDS.has(entry.kind) ? (entry.kind as Question['kind']) : 'choice';
    const options = Array.isArray(entry.options)
      ? entry.options
          .filter((option: any) => option && typeof option.label === 'string' && option.label)
          .map((option: any) => ({ label: option.label as string, description: typeof option.description === 'string' ? option.description : '' }))
      : [];
    // Claude Code itself also rejects choice questions without options, and duplicate question texts / options
    // (answers are returned keyed by question text, so with duplicates one of the answers would be lost)
    if (kind === 'choice' && options.length === 0) return null;
    if (questions.some((other) => other.question === entry.question)) return null;
    if (new Set(options.map((option: { label: string }) => option.label)).size !== options.length) return null;
    questions.push({
      question: entry.question,
      header: typeof entry.header === 'string' ? entry.header : '',
      description: typeof entry.description === 'string' ? entry.description : '',
      kind,
      options,
      multiSelect: entry.multiSelect === true,
      placeholder: typeof entry.placeholder === 'string' ? entry.placeholder : '',
      min: optionalNumber(entry.min),
      max: optionalNumber(entry.max),
      step: optionalNumber(entry.step),
      unit: typeof entry.unit === 'string' ? entry.unit : '',
    });
  }
  return questions;
}

export function emptyDraft(question: Question, input: Record<string, unknown>): QuestionDraft {
  const raw = (input.questions as any[])?.find?.((entry) => entry?.question === question.question);
  const initial = optionalNumber(raw?.defaultValue);
  return { selected: [], text: question.kind === 'number' && initial !== null ? String(initial) : '' };
}

// The answer to one question. null if it is not an answer. Multi-select is comma-separated (the same shape as Claude Code's tool_result)
export function formatAnswer(question: Question, draft: QuestionDraft): string | null {
  const text = draft.text.trim();
  if (question.kind === 'text') return text || null;
  if (question.kind === 'number') {
    const value = Number(text);
    if (!text || !Number.isFinite(value)) return null;
    if ((question.min !== null && value < question.min) || (question.max !== null && value > question.max)) return null;
    return text;
  }
  // In single-select, free text replaces the option (toggleOption / setText make sure both are never kept)
  const labels = question.options.map((option) => option.label).filter((label) => draft.selected.includes(label));
  if (!question.multiSelect) return text || labels[0] || null;
  // Ordered as the options are, with free text appended last
  if (text) labels.push(text);
  return labels.length > 0 ? labels.join(', ') : null;
}

// Answers to all questions (drafts is in the same order as questions). null if any question is unanswered
export function collectAnswers(questions: Question[], drafts: QuestionDraft[]): Record<string, string> | null {
  // The key is the question text itself. Stored in a prototype-less object so that even a question named "__proto__" becomes an own property
  const answers: Record<string, string> = Object.create(null);
  for (const [index, question] of questions.entries()) {
    const answer = formatAnswer(question, drafts[index]);
    if (answer === null) return null;
    answers[question.question] = answer;
  }
  return answers;
}

// Toggles the selection when an option is pressed. Single-select replaces (pressing the same one deselects) and also clears free text. Multi-select flips
export function toggleOption(question: Question, draft: QuestionDraft, label: string): void {
  if (draft.selected.includes(label)) {
    draft.selected = draft.selected.filter((selected) => selected !== label);
  } else if (question.multiSelect) {
    draft.selected = [...draft.selected, label];
  } else {
    draft.selected = [label];
    draft.text = '';
  }
}

// Text in the input field. In single-select, free text replaces the option, so the selection is cleared once typing starts
export function setText(question: Question, draft: QuestionDraft, text: string): void {
  draft.text = text;
  if (question.kind === 'choice' && !question.multiSelect && text) draft.selected = [];
}
