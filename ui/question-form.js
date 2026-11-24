// The form that answers an agent's question, shared by the note and run dialogs. ui/question.js
// has the rules that read the question's fields and build the answer.

import { el } from "./dom.js";
import { questionContent, questionFields } from "./question.js";
import { foldRun, isLive, openQuestion } from "./run.js";

// The open question of `run`, while it is live, as `{ key, form }`; null when it asks none.
// `kept` is what the dialog's last draw got from here: while the question stays the same, its
// form is the same node, so a redraw does not drop what the engineer has chosen or typed.
// `answer(request, content)` sends the reply. The key holds the run's id: each agent numbers
// its requests from 0, and a thread's runs can have different agents.
export function keptQuestion(kept, run, answer) {
  const question = isLive(run) ? openQuestion(foldRun(run.entries).items) : null;
  if (!question) return null;
  const key = `${run.id}:${question.request}`;
  if (kept?.key === key) return kept;
  const form = questionForm(question, run.agent, (content) => answer(question.request, content));
  return { key, form };
}

// The agent's open question as a form (board 9 on the design canvas): its message, a field
// per form field, Skip and Send answer (Cmd-Enter). `answer(content)` sends the reply: the
// form's values, or `null` to skip; it resolves to whether the reply was taken. Shared by
// the note and run dialogs.
export function questionForm(question, agent, answer) {
  const fields = questionFields(question.fields);
  const values = {};
  const form = el("form", "question");
  form.append(
    el("div", "question-label", `${agent} has a question`),
    el("div", "question-message", question.message),
  );
  fields.forEach((field, index) => form.append(fieldView(field, index, values)));
  const actions = el("div", "question-actions");
  const skip = el("button", "button", `Skip, let ${agent} decide`);
  skip.type = "button";
  const send = el("button", "button primary");
  send.type = "submit";
  send.append("Send answer", el("kbd", "", "⌘↵"));
  actions.append(skip, el("span", "spacer"), send);
  form.append(actions);
  // Sent once: the buttons stay disabled while the reply is on its way, and the run's next
  // draw replaces the form. A reply that was not taken (the post failed) can be sent again.
  const reply = async (content) => {
    skip.disabled = true;
    send.disabled = true;
    if (await answer(content)) return;
    skip.disabled = false;
    send.disabled = false;
  };
  skip.addEventListener("click", () => reply(null));
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    reply(questionContent(fields, values));
  });
  form.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && event.metaKey) {
      event.preventDefault();
      event.stopPropagation();
      form.requestSubmit();
    }
  });
  return form;
}

// One field of a question's form; what the engineer enters goes into `values` by key.
function fieldView(field, index, values) {
  const part = el("fieldset", `question-field ${field.kind}`);
  part.append(el("legend", "question-title", field.title));
  if (field.description) part.append(el("div", "question-description", field.description));
  const name = `question-${index}`;
  if (field.kind === "one" || field.kind === "many" || field.kind === "yesno") {
    const options =
      field.kind === "yesno"
        ? [
            { value: true, title: "Yes", description: "" },
            { value: false, title: "No", description: "" },
          ]
        : field.options;
    const chosen = new Set();
    for (const option of options) {
      const choice = el("label", "question-choice");
      const input = el("input");
      input.type = field.kind === "many" ? "checkbox" : "radio";
      input.name = name;
      input.addEventListener("change", () => {
        if (field.kind === "many") {
          if (input.checked) chosen.add(option.value);
          else chosen.delete(option.value);
          values[field.key] = [...chosen];
        } else values[field.key] = option.value;
      });
      const text = el("span", "question-choice-text");
      text.append(el("span", "question-choice-title", option.title));
      if (option.description)
        text.append(el("span", "question-choice-description", option.description));
      choice.append(input, text);
      part.append(choice);
    }
    return part;
  }
  const input = field.kind === "text" ? el("textarea") : el("input");
  if (field.kind === "text") input.rows = 2;
  else input.type = "number";
  input.className = "field question-input";
  input.setAttribute("aria-label", field.title);
  input.addEventListener("input", () => {
    values[field.key] = input.value;
  });
  part.append(input);
  return part;
}
