import eslint from "@eslint/js";
import globals from "globals";
import tseslint from "typescript-eslint";

export default tseslint.config(
  {
    ignores: [
      "coverage/**",
      "dist/**",
      "node_modules/**",
      "**/.venv/**",
      ".zvec-grep/**",
      "*.tgz",
    ],
  },
  eslint.configs.recommended,
  ...tseslint.configs.recommendedTypeChecked,
  {
    files: ["src/**/*.ts"],
    languageOptions: {
      globals: globals.node,
      parserOptions: {
        projectService: true,
        tsconfigRootDir: import.meta.dirname,
      },
    },
    rules: {
      "no-console": "off",
      "@typescript-eslint/consistent-type-imports": [
        "error",
        { fixStyle: "inline-type-imports" },
      ],
      "@typescript-eslint/no-confusing-void-expression": "off",
      "@typescript-eslint/no-base-to-string": "off",
      "@typescript-eslint/no-misused-promises": [
        "error",
        { checksVoidReturn: false },
      ],
      "@typescript-eslint/no-unsafe-argument": "off",
      "@typescript-eslint/no-unsafe-assignment": "off",
      "@typescript-eslint/no-unsafe-call": "off",
      "@typescript-eslint/no-unsafe-member-access": "off",
      "@typescript-eslint/no-unused-vars": [
        "error",
        {
          argsIgnorePattern: "^_",
          varsIgnorePattern:
            "^(_|truncateInline|findNearestAnonymousWorkspace)",
        },
      ],
      "@typescript-eslint/no-unnecessary-type-assertion": "off",
      "@typescript-eslint/only-throw-error": "off",
      "@typescript-eslint/require-await": "off",
      "@typescript-eslint/unbound-method": "off",
      "no-useless-assignment": "off",
    },
  },
  {
    files: ["src/**/*.ts"],
    ignores: ["src/engine/extraction/**/*.ts", "src/engine/models/**/*.ts"],
    rules: {
      "no-restricted-imports": [
        "error",
        {
          patterns: [
            {
              regex: "(^|/)models/(?!index\\.js$)",
              message:
                "Import the models module through models/index.ts outside src/engine/models.",
            },
            {
              regex: "(^|/)extraction/(?!index\\.js$)",
              message:
                "Import the extraction module through extraction/index.ts outside src/engine/extraction.",
            },
          ],
        },
      ],
    },
  },
  {
    files: ["**/*.js", "**/*.mjs"],
    extends: [tseslint.configs.disableTypeChecked],
    languageOptions: {
      globals: globals.node,
    },
    rules: {
      "no-console": "off",
      "no-control-regex": "off",
    },
  },
);
