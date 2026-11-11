import { createRequire } from "node:module";
import type * as SDK from "@zvec/zvec";

export type ZVecCollectionSchema = SDK.ZVecCollectionSchema;
export type ZVecMetricType = SDK.ZVecMetricType;

// Load our pinned source build explicitly. A missing build must not silently
// select @zvec/zvec's platform package, which contains a different native core.
const binding = createRequire(import.meta.url)(
  "../../../native/zvec_node_binding.node",
);

export const ZVecCollectionSchema: typeof SDK.ZVecCollectionSchema =
  binding.CollectionSchema;
export const ZVecCreateAndOpen: typeof SDK.ZVecCreateAndOpen =
  binding.createAndOpen;
export const ZVecDataType: typeof SDK.ZVecDataType = binding.DataType;
export const ZVecIndexType: typeof SDK.ZVecIndexType = binding.IndexType;
export const ZVecInitialize: typeof SDK.ZVecInitialize = binding.initialize;
export const ZVecLogLevel: typeof SDK.ZVecLogLevel = binding.LogLevel;
export const ZVecMetricType: typeof SDK.ZVecMetricType = binding.MetricType;
export const ZVecOpen: typeof SDK.ZVecOpen = binding.open;
export type {
  ZVecCollection,
  ZVecDoc,
  ZVecDocInput,
  ZVecStatus,
} from "@zvec/zvec";
