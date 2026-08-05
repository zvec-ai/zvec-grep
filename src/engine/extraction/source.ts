import { EngineError } from "../errors/index.js";
import type { FileInfo, ImageFormat } from "../types.js";

export type SourceKind = "text" | "image";

type SourceBase = {
  kind: SourceKind;
  file: FileInfo;
};

export type TextSource = SourceBase & {
  kind: "text";
  text: string;
};

export type ImageSource = SourceBase & {
  kind: "image";
  data: Uint8Array;
  format: ImageFormat;
};

export type Source = TextSource | ImageSource;

export function validateSourceFile(source: Source): void {
  if (source.file.id.trim().length === 0) {
    throw new EngineError("Extractor source requires a non-empty file id", {
      code: "ZVEC_GREP.ENGINE.EXTRACTORS.EMPTY_FILE_ID",
      context: `sourceKind=${source.kind}`,
    });
  }

  if (source.file.absolutePath.trim().length === 0) {
    throw new EngineError(
      "Extractor source requires a non-empty absolute file path",
      {
        code: "ZVEC_GREP.ENGINE.EXTRACTORS.EMPTY_ABSOLUTE_FILE_PATH",
        context: `fileId=${source.file.id} sourceKind=${source.kind}`,
      },
    );
  }

  if (source.file.relativePath.trim().length === 0) {
    throw new EngineError(
      "Extractor source requires a non-empty relative file path",
      {
        code: "ZVEC_GREP.ENGINE.EXTRACTORS.EMPTY_RELATIVE_FILE_PATH",
        context: `fileId=${source.file.id} sourceKind=${source.kind}`,
      },
    );
  }
}
