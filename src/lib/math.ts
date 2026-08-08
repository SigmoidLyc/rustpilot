function escapeHtml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/\"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

const SYMBOLS: Record<string, string> = {
  alpha: "&alpha;",
  beta: "&beta;",
  gamma: "&gamma;",
  delta: "&delta;",
  epsilon: "&epsilon;",
  varepsilon: "&#x03F5;",
  zeta: "&zeta;",
  eta: "&eta;",
  theta: "&theta;",
  vartheta: "&#x03D1;",
  iota: "&iota;",
  kappa: "&kappa;",
  lambda: "&lambda;",
  mu: "&mu;",
  nu: "&nu;",
  xi: "&xi;",
  pi: "&pi;",
  varpi: "&#x03D6;",
  rho: "&rho;",
  varrho: "&rho;",
  sigma: "&sigma;",
  varsigma: "&#x03C2;",
  tau: "&tau;",
  upsilon: "&upsilon;",
  phi: "&phi;",
  varphi: "&#x03D5;",
  chi: "&chi;",
  psi: "&psi;",
  omega: "&omega;",
  Gamma: "&Gamma;",
  Delta: "&Delta;",
  Theta: "&Theta;",
  Lambda: "&Lambda;",
  Xi: "&Xi;",
  Pi: "&Pi;",
  Sigma: "&Sigma;",
  Upsilon: "&Upsilon;",
  Phi: "&Phi;",
  Psi: "&Psi;",
  Omega: "&Omega;",
  infty: "&#x221E;",
  partial: "&part;",
  nabla: "&nabla;",
  ell: "&ell;",
  hbar: "&#x210F;",
  pm: "&plusmn;",
  mp: "&#x2213;",
  times: "&times;",
  div: "&divide;",
  cdot: "&#x22C5;",
  ast: "&#x2217;",
  star: "&#x22C6;",
  circ: "&#x2218;",
  bullet: "&#x2022;",
  le: "&le;",
  leq: "&le;",
  ge: "&ge;",
  geq: "&ge;",
  neq: "&ne;",
  ne: "&ne;",
  approx: "&asymp;",
  sim: "&sim;",
  equiv: "&equiv;",
  cong: "&cong;",
  propto: "&#x221D;",
  to: "&rarr;",
  rightarrow: "&rarr;",
  leftarrow: "&larr;",
  leftrightarrow: "&harr;",
  Rightarrow: "&rArr;",
  Leftarrow: "&lArr;",
  Leftrightarrow: "&hArr;",
  mapsto: "&#x21A6;",
  in: "&isin;",
  notin: "&notin;",
  ni: "&ni;",
  subset: "&#x2282;",
  subseteq: "&#x2286;",
  supset: "&#x2283;",
  supseteq: "&#x2287;",
  cup: "&#x222A;",
  cap: "&#x2229;",
  forall: "&#x2200;",
  exists: "&#x2203;",
  emptyset: "&#x2205;",
  varnothing: "&#x2205;",
  land: "&#x2227;",
  lor: "&#x2228;",
  neg: "&#x00AC;",
  sum: "&#x2211;",
  prod: "&#x220F;",
  coprod: "&#x2210;",
  int: "&#x222B;",
  iint: "&#x222C;",
  iiint: "&#x222D;",
  oint: "&#x222E;",
  lim: "lim",
  min: "min",
  max: "max",
  sup: "sup",
  inf: "inf",
  det: "det",
  ker: "ker",
  dim: "dim",
  log: "log",
  ln: "ln",
  lg: "lg",
  sin: "sin",
  cos: "cos",
  tan: "tan",
  cot: "cot",
  sec: "sec",
  csc: "csc",
  sinh: "sinh",
  cosh: "cosh",
  tanh: "tanh"
};

const DELIMITERS: Record<string, string> = {
  "(": "(",
  ")": ")",
  "[": "[",
  "]": "]",
  "{": "{",
  "}": "}",
  "|": "|",
  "langle": "&#x27E8;",
  "rangle": "&#x27E9;",
  "lfloor": "&#x230A;",
  "rfloor": "&#x230B;",
  "lceil": "&#x2308;",
  "rceil": "&#x2309;",
  "lvert": "|",
  "rvert": "|",
  "Vert": "&#x2016;",
  "vert": "|",
  "middle": "|"
};

const ENVIRONMENT_FENCES: Record<string, [string, string]> = {
  pmatrix: ["(", ")"],
  bmatrix: ["[", "]"],
  Bmatrix: ["{", "}"],
  vmatrix: ["|", "|"],
  Vmatrix: ["&#x2016;", "&#x2016;"],
  cases: ["{", ""],
  matrix: ["", ""],
  smallmatrix: ["", ""],
  aligned: ["", ""],
  array: ["", ""]
};

const DECORATIONS: Record<string, string> = {
  hat: "^",
  widehat: "^",
  bar: "&#x00AF;",
  overline: "&#x00AF;",
  vec: "&#x2192;",
  tilde: "&#x02DC;",
  widetilde: "&#x02DC;",
  dot: ".",
  ddot: "..",
  overbrace: "&#x23DE;",
  underline: "&#x0332;",
  underbrace: "&#x23DF;"
};

const VARIANTS: Record<string, string> = {
  mathbb: "double-struck",
  mathcal: "script",
  mathfrak: "fraktur",
  mathrm: "normal",
  textnormal: "normal",
  mathbf: "bold",
  boldsymbol: "bold"
};

type MathFragment = {
  html: string;
  kind: "atom" | "operator";
};

function mathText(value: string, kind: MathFragment["kind"] = "atom"): MathFragment {
  const tag = kind === "operator" ? "mo" : "mi";
  return { html: `<${tag}>${escapeHtml(value)}</${tag}>`, kind };
}

function operator(value: string, attributes = ""): MathFragment {
  return { html: `<mo${attributes}>${value}</mo>`, kind: "operator" };
}

function emptyRow(): string {
  return "<mrow></mrow>";
}

class MathParser {
  private position = 0;

  public constructor(private readonly source: string) {}

  public parse(): string {
    return this.parseExpression();
  }

  private peek(): string {
    return this.source[this.position] ?? "";
  }

  private skipWhitespace(): void {
    while (/\s/.test(this.peek())) this.position += 1;
  }

  private parseExpression(stop?: string): string {
    const nodes: string[] = [];
    while (this.position < this.source.length) {
      this.skipWhitespace();
      const current = this.peek();
      if (!current) break;
      if (stop && current === stop) {
        this.position += 1;
        break;
      }
      if (current === "}") {
        this.position += 1;
        break;
      }

      const start = this.position;
      const atom = this.parseAtom();
      if (!atom) {
        if (this.position === start) this.position += 1;
        continue;
      }

      let subscript = "";
      let superscript = "";
      while (this.peek() === "_" || this.peek() === "^") {
        const marker = this.peek();
        this.position += 1;
        const script = this.parseScript();
        if (marker === "_") subscript = script;
        else superscript = script;
      }
      nodes.push(this.withScripts(atom.html, subscript, superscript));
    }
    return nodes.join("");
  }

  private withScripts(base: string, subscript: string, superscript: string): string {
    if (subscript && superscript) return `<msubsup>${base}${subscript}${superscript}</msubsup>`;
    if (subscript) return `<msub>${base}${subscript}</msub>`;
    if (superscript) return `<msup>${base}${superscript}</msup>`;
    return base;
  }

  private parseScript(): string {
    this.skipWhitespace();
    if (this.peek() === "{") return this.parseGroup();
    const atom = this.parseAtom();
    return atom?.html ?? emptyRow();
  }

  private parseAtom(): MathFragment | null {
    this.skipWhitespace();
    const current = this.peek();
    if (!current) return null;

    if (current === "{") return { html: this.parseGroup(), kind: "atom" };
    if (current === "\\") return this.parseCommand();
    if (current === "}") return null;
    if (current === "&") {
      this.position += 1;
      return operator("&amp;");
    }

    const number = this.source.slice(this.position).match(/^(?:\d+(?:\.\d*)?|\.\d+)/)?.[0];
    if (number) {
      this.position += number.length;
      return { html: `<mn>${number}</mn>`, kind: "atom" };
    }

    if (/[A-Za-z]/.test(current)) {
      this.position += 1;
      return mathText(current);
    }

    this.position += 1;
    if (current === "'") return operator("&#x2032;");
    if (current === "-") return operator("&#x2212;");
    if (current === "*") return operator("&#x2217;");
    if (current === "<") return operator("&lt;");
    if (current === ">") return operator("&gt;");
    return operator(escapeHtml(current));
  }

  private parseGroup(): string {
    if (this.peek() === "{") this.position += 1;
    return `<mrow>${this.parseExpression("}")}</mrow>`;
  }

  private parseRequiredGroup(): string {
    this.skipWhitespace();
    if (this.peek() === "{") return this.parseGroup();
    const atom = this.parseAtom();
    return atom?.html ?? emptyRow();
  }

  private readRawGroup(): string {
    this.skipWhitespace();
    if (this.peek() !== "{") return "";
    this.position += 1;
    const start = this.position;
    let depth = 1;
    while (this.position < this.source.length && depth > 0) {
      const current = this.source[this.position];
      if (current === "{") depth += 1;
      if (current === "}") depth -= 1;
      this.position += 1;
    }
    const end = depth === 0 ? this.position - 1 : this.position;
    return this.source.slice(start, end);
  }

  private parseCommand(): MathFragment | null {
    this.position += 1;
    if (!this.peek()) return operator("\\");

    if (!/[A-Za-z]/.test(this.peek())) {
      const escaped = this.peek();
      this.position += 1;
      if (escaped === " ") return { html: '<mspace width="0.333em" />', kind: "atom" };
      const literal = escaped === "{" || escaped === "}" ? escapeHtml(escaped) : escapeHtml(escaped);
      return operator(literal);
    }

    const start = this.position;
    while (/[A-Za-z]/.test(this.peek())) this.position += 1;
    const name = this.source.slice(start, this.position);

    if (name === "frac" || name === "dfrac" || name === "tfrac") {
      const numerator = this.parseRequiredGroup();
      const denominator = this.parseRequiredGroup();
      return { html: `<mfrac>${numerator}${denominator}</mfrac>`, kind: "atom" };
    }

    if (name === "sqrt") {
      this.skipWhitespace();
      let index = "";
      if (this.peek() === "[") {
        this.position += 1;
        const startIndex = this.position;
        while (this.position < this.source.length && this.peek() !== "]") this.position += 1;
        index = new MathParser(this.source.slice(startIndex, this.position)).parse();
        if (this.peek() === "]") this.position += 1;
      }
      const body = this.parseRequiredGroup();
      return index
        ? { html: `<mroot>${body}${index}</mroot>`, kind: "atom" }
        : { html: `<msqrt>${body}</msqrt>`, kind: "atom" };
    }

    if (name === "binom") {
      const top = this.parseRequiredGroup();
      const bottom = this.parseRequiredGroup();
      return {
        html: `<mrow><mo>(</mo><mfrac linethickness="0">${top}${bottom}</mfrac><mo>)</mo></mrow>`,
        kind: "atom"
      };
    }

    if (name === "overset" || name === "stackrel") {
      const label = this.parseRequiredGroup();
      const body = this.parseRequiredGroup();
      return { html: `<mover>${body}${label}</mover>`, kind: "atom" };
    }

    if (name === "underset") {
      const label = this.parseRequiredGroup();
      const body = this.parseRequiredGroup();
      return { html: `<munder>${body}${label}</munder>`, kind: "atom" };
    }

    if (DECORATIONS[name]) {
      const body = this.parseRequiredGroup();
      const tag = name.startsWith("under") ? "munder" : "mover";
      const accent = DECORATIONS[name];
      return { html: `<${tag} accent="true">${body}<mo>${accent}</mo></${tag}>`, kind: "atom" };
    }

    if (VARIANTS[name]) {
      const body = this.parseRequiredGroup();
      return { html: `<mrow mathvariant="${VARIANTS[name]}">${body}</mrow>`, kind: "atom" };
    }

    if (name === "text" || name === "textrm") {
      const value = this.readRawGroup();
      return { html: `<mtext>${escapeHtml(value).replace(/\s+/g, " ")}</mtext>`, kind: "atom" };
    }

    if (name === "operatorname") {
      const value = this.readRawGroup();
      return { html: `<mo>${escapeHtml(value).replace(/\s+/g, " ")}</mo>`, kind: "operator" };
    }

    if (name === "begin") return { html: this.parseEnvironment(), kind: "atom" };
    if (name === "left" || name === "right" || name === "middle") {
      const delimiter = this.readDelimiter();
      if (!delimiter) return { html: "", kind: "operator" };
      return operator(delimiter, ' stretchy="true"');
    }

    if (
      name === "displaystyle" ||
      name === "textstyle" ||
      name === "scriptstyle" ||
      name === "scriptscriptstyle" ||
      name === "limits" ||
      name === "nolimits" ||
      name === "," ||
      name === ";" ||
      name === ":" ||
      name === "!"
    ) {
      return { html: '<mspace width="0.167em" />', kind: "atom" };
    }

    const symbol = SYMBOLS[name];
    if (symbol) {
      const isLargeOperator = ["sum", "prod", "coprod", "int", "iint", "iiint", "oint", "lim"].includes(name);
      return operator(symbol, isLargeOperator ? ' movablelimits="true"' : "");
    }

    if (name === "lvert" || name === "rvert" || name === "vert" || name === "Vert") {
      return operator(DELIMITERS[name] ?? "|");
    }

    return { html: `<mtext>${escapeHtml(`\\${name}`)}</mtext>`, kind: "atom" };
  }

  private readDelimiter(): string {
    this.skipWhitespace();
    if (this.peek() !== "\\") {
      const delimiter = this.peek();
      this.position += delimiter ? 1 : 0;
      if (delimiter === ".") return "";
      return DELIMITERS[delimiter] ?? escapeHtml(delimiter);
    }

    this.position += 1;
    if (!this.peek()) return "";
    if (!/[A-Za-z]/.test(this.peek())) {
      const delimiter = this.peek();
      this.position += 1;
      if (delimiter === ".") return "";
      return DELIMITERS[delimiter] ?? escapeHtml(delimiter);
    }

    const start = this.position;
    while (/[A-Za-z]/.test(this.peek())) this.position += 1;
    const name = this.source.slice(start, this.position);
    return DELIMITERS[name] ?? SYMBOLS[name] ?? escapeHtml(name);
  }

  private parseEnvironment(): string {
    const environment = this.readRawGroup();
    const endMarker = `\\end{${environment}}`;
    const end = this.source.indexOf(endMarker, this.position);
    if (end < 0) return `<mtext>${escapeHtml(`\\begin{${environment}}`)}</mtext>`;

    const body = this.source.slice(this.position, end);
    this.position = end + endMarker.length;
    if (environment === "array") this.readRawGroup();

    const rows = body
      .split(/\\\\/)
      .map((row) => row.replace(/^\s*\\hline\s*/, "").replace(/^\s*\[[^\]]*\]\s*/, "").trim())
      .filter(Boolean)
      .map((row) => row.split("&").map((cell) => new MathParser(cell).parse() || emptyRow()));
    const table = `<mtable rowspacing="0.3em" columnalign="center">${rows
      .map((row) => `<mtr>${row.map((cell) => `<mtd>${cell}</mtd>`).join("")}</mtr>`)
      .join("")}</mtable>`;
    const [left, right] = ENVIRONMENT_FENCES[environment] ?? ["", ""];
    const fenced = `${left ? `<mo stretchy="true">${left}</mo>` : ""}${table}${right ? `<mo stretchy="true">${right}</mo>` : ""}`;
    return `<mrow>${fenced}</mrow>`;
  }
}

export function renderMath(latex: string, display: boolean): string {
  const source = latex.trim();
  const content = new MathParser(source).parse() || `<mtext>${escapeHtml(source)}</mtext>`;
  const className = display ? "math-display" : "math-inline";
  const displayAttribute = display ? ' display="block"' : "";
  const label = escapeHtml(source.replace(/\s+/g, " "));
  return `<math class="${className}" xmlns="http://www.w3.org/1998/Math/MathML"${displayAttribute} aria-label="${label}"><mrow>${content}</mrow></math>`;
}
