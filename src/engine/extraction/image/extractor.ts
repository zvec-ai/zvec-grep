import { EngineError } from "../../errors/index.js";
import type { EntityFragment } from "../../types.js";
import { makeEntityId } from "../ids.js";
import { validateSourceFile, type Source } from "../source.js";

export class ImageExtractor {
  async extract(source: Source): Promise<EntityFragment[]> {
    if (source.kind !== "image") {
      throw new EngineError("Image extractor received a non-image source", {
        code: "ZVEC_GREP.ENGINE.EXTRACTORS.IMAGE_UNSUPPORTED_SOURCE",
        context: `fileId=${source.file.id} sourceKind=${source.kind}`,
      });
    }

    validateSourceFile(source);

    if (source.data.byteLength === 0) {
      throw new EngineError("Image extractor requires non-empty image data", {
        code: "ZVEC_GREP.ENGINE.EXTRACTORS.IMAGE_EMPTY_DATA",
        context: `fileId=${source.file.id} format=${source.format}`,
      });
    }

    return [
      {
        id: makeEntityId(source.file.id, 0),
        fileId: source.file.id,
        range: {
          kind: "file",
        },
        content: {
          kind: "image",
          data: source.data,
          format: source.format,
        },
      },
    ];
  }
}
