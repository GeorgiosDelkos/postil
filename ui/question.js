// An agent's question as a form (ACP form elicitation): the fields its JSON Schema describes,
// and the reply's content from what the engineer chose. Plain functions, kept apart from the
// document so `node --test` runs them; note-dialog.js draws the form.
//
// The schema is an object whose properties are the fields. Claude's adapter asks with one
// single-select field per question (`question_0`, a string with `oneOf` choices) followed by
// its free-text companion (`question_0_custom`, titled "Other"), and a multi-select as an
// array whose `items` hold `anyOf` choices. A plain JSON Schema `enum` is read as choices too.

// The form's fields in the schema's order. Each is `{ key, kind, title, description, options }`:
// `kind` is "one" (pick one), "many" (pick any), "text", "number" or "yesno"; `options` are
// `{ value, title, description }` for the first two. A field of a type the page cannot draw
// is left out: its value is then not sent, as for a field left empty.
export function questionFields(schema) {
  const properties = schema?.properties;
  if (!properties || typeof properties !== "object") return [];
  const fields = [];
  for (const [key, field] of Object.entries(properties)) {
    if (!field || typeof field !== "object") continue;
    const title = typeof field.title === "string" ? field.title : key;
    const description = typeof field.description === "string" ? field.description : "";
    const base = { key, title, description };
    if (field.type === "array") {
      const options = choices(field.items);
      if (options.length) fields.push({ ...base, kind: "many", options });
    } else if (field.type === "string" || field.type === undefined) {
      const options = choices(field);
      fields.push(options.length ? { ...base, kind: "one", options } : { ...base, kind: "text", options: [] });
    } else if (field.type === "number" || field.type === "integer") {
      fields.push({ ...base, kind: "number", options: [] });
    } else if (field.type === "boolean") {
      fields.push({ ...base, kind: "yesno", options: [] });
    }
  }
  return fields;
}

// A field's choices: `oneOf` or `anyOf` entries with a `const`, else a plain `enum`.
function choices(field) {
  if (!field || typeof field !== "object") return [];
  const listed = field.oneOf ?? field.anyOf;
  if (Array.isArray(listed)) {
    return listed
      .filter((option) => option && "const" in option)
      .map((option) => ({
        value: option.const,
        title: typeof option.title === "string" ? option.title : String(option.const),
        description: typeof option.description === "string" ? option.description : "",
      }));
  }
  if (Array.isArray(field.enum)) {
    return field.enum.map((value) => ({ value, title: String(value), description: "" }));
  }
  return [];
}

// The reply's content from `values`, the engineer's input by field key: a chosen value, a
// list of them, typed text, or a yes/no. Empty input is left out, so a field the engineer
// skipped is absent, as Claude's adapter reads it ("skipped"). A number that does not parse
// is left out too.
export function questionContent(fields, values) {
  const content = {};
  for (const field of fields) {
    const value = values[field.key];
    switch (field.kind) {
      case "many":
        if (Array.isArray(value) && value.length) content[field.key] = value;
        break;
      case "text":
        if (typeof value === "string" && value.trim()) content[field.key] = value.trim();
        break;
      case "number": {
        const number = typeof value === "string" && value.trim() ? Number(value) : NaN;
        if (Number.isFinite(number)) content[field.key] = number;
        break;
      }
      case "yesno":
        if (typeof value === "boolean") content[field.key] = value;
        break;
      default:
        if (value !== undefined && value !== null && value !== "") content[field.key] = value;
    }
  }
  return content;
}
