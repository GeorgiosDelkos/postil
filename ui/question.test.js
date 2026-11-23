import assert from "node:assert/strict";
import test from "node:test";

import { questionContent, questionFields } from "./question.js";
import { foldRun, openQuestion } from "./run.js";

// Two questions as Claude's adapter asks them (src/elicitation.ts,
// askUserQuestionsToCreateRequest).
const claude = {
  type: "object",
  properties: {
    question_0: {
      type: "string",
      title: "Overflow",
      description: "Should divide(i32::MIN, -1) also return None?",
      oneOf: [
        { const: "Yes", title: "Return None", description: "Use checked_div." },
        { const: "No", title: "Keep the panic" },
      ],
    },
    question_0_custom: { type: "string", title: "Other", description: "Type your own answer." },
    question_1: {
      type: "array",
      title: "Callers",
      items: {
        anyOf: [
          { const: "main", title: "main" },
          { const: "tests", title: "tests" },
        ],
      },
    },
  },
};

test("a form's fields follow the schema: one-of, free text, any-of", () => {
  const fields = questionFields(claude);
  assert.deepEqual(
    fields.map((field) => [field.key, field.kind, field.title]),
    [
      ["question_0", "one", "Overflow"],
      ["question_0_custom", "text", "Other"],
      ["question_1", "many", "Callers"],
    ],
  );
  assert.deepEqual(fields[0].options[0], {
    value: "Yes",
    title: "Return None",
    description: "Use checked_div.",
  });
  assert.equal(fields[0].description, "Should divide(i32::MIN, -1) also return None?");
});

test("plain JSON Schema fields are read too, and unknown ones are left out", () => {
  const fields = questionFields({
    type: "object",
    properties: {
      color: { type: "string", enum: ["red", "blue"] },
      count: { type: "integer" },
      sure: { type: "boolean", title: "Sure?" },
      shape: { type: "object" },
    },
  });
  assert.deepEqual(
    fields.map((field) => [field.key, field.kind]),
    [
      ["color", "one"],
      ["count", "number"],
      ["sure", "yesno"],
    ],
  );
  assert.deepEqual(
    fields[0].options.map((option) => option.value),
    ["red", "blue"],
  );
  assert.equal(fields[1].title, "count", "a field without a title is named by its key");
  assert.deepEqual(questionFields({ type: "object" }), [], "no properties, no fields");
  assert.deepEqual(questionFields(null), []);
});

test("the reply holds what was chosen or typed, and leaves skipped fields out", () => {
  const fields = questionFields(claude);
  assert.deepEqual(
    questionContent(fields, {
      question_0: "Yes",
      question_0_custom: "  and log it ",
      question_1: ["main"],
    }),
    { question_0: "Yes", question_0_custom: "and log it", question_1: ["main"] },
  );
  assert.deepEqual(
    questionContent(fields, { question_0_custom: "   ", question_1: [] }),
    {},
    "nothing chosen",
  );
  const plain = questionFields({
    type: "object",
    properties: { n: { type: "number" }, ok: { type: "boolean" } },
  });
  assert.deepEqual(questionContent(plain, { n: "2.5", ok: false }), { n: 2.5, ok: false });
  assert.deepEqual(
    questionContent(plain, { n: "two" }),
    {},
    "a number that does not parse is left out",
  );
});

test("a run's question is open until it is answered, skipped, or closed", () => {
  const asked = { kind: "question", request: 3, message: "Which?", fields: claude, toolCall: null };
  const open = foldRun([asked]).items;
  assert.equal(openQuestion(open)?.request, 3);
  const answered = foldRun([
    asked,
    { kind: "reply", request: 3, content: { question_0: "No" } },
  ]).items;
  assert.equal(openQuestion(answered), null);
  assert.deepEqual(answered[0].answer, { content: { question_0: "No" } });
  const skipped = foldRun([asked, { kind: "reply", request: 3, content: null }]).items;
  assert.deepEqual(skipped[0].answer, { content: null });
  const closed = foldRun([asked, { kind: "answer", request: 3, option: null }]).items;
  assert.equal(closed[0].answer, null, "the run stopped, or the agent withdrew it");
});
