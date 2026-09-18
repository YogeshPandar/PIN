#ifndef PIN_STORAGE_H
#define PIN_STORAGE_H

#include "postgres.h"
#include "access/amapi.h"
#include "access/tableam.h"
#include "nodes/tidbitmap.h"
#include "storage/buf.h"

/* pgrx guards every call; pointers never outlive the synchronous operation. */
extern void pin_storage_check(Relation index, Relation heap, struct IndexInfo *info);
extern void pin_writer_lock(Relation index);
extern void pin_writer_unlock(Relation index);
extern void pin_structure_lock(Relation index, bool exclusive);
extern void pin_structure_unlock(Relation index, bool exclusive);
extern uint32 pin_storage_blocks(Relation index);
extern uint32 pin_storage_extend(Relation index);
extern uint32 pin_storage_read(Relation index, uint32 block, uint8 *out, uint32 capacity);
extern uint32 pin_storage_owner_read(Relation index, uint32 block, uint8 *out, uint32 capacity,
                                      Buffer *held);
extern void pin_storage_commit(Relation index, uint32 count, const uint32 *blocks,
                               const uint8 *const *bytes, const uint32 *lengths,
                               const bool *full_images);
extern void pin_storage_interrupt(void);
extern void pin_root_coordinates(ItemPointer tid, uint32 *block, uint16 *offset);
extern double pin_heap_build_scan(Relation heap, Relation index, struct IndexInfo *info,
                                  IndexBuildCallback callback, void *state);
extern void pin_bitmap_add(TIDBitmap *bitmap, uint32 count,
                           const uint32 *blocks, const uint16 *offsets);
extern bool pin_vacuum_removable(IndexBulkDeleteCallback callback, void *state,
                                 uint32 block, uint16 offset);
extern IndexScanDesc pin_scan_begin(Relation index, int nkeys, int norderbys);
extern void pin_scan_end(IndexScanDesc scan);
extern void pin_scan_validate(IndexScanDesc scan);
extern void pin_scan_rescan(IndexScanDesc scan, ScanKey keys, int nkeys, int norderbys);
extern bool pin_opclass_validate(Oid opclass);
extern void pin_opclass_adjust(Oid opclass, List *operators, List *functions);
extern void pin_index_cost(struct PlannerInfo *root, struct IndexPath *path, double loops,
                           Cost *startup, Cost *total, Selectivity *selectivity,
                           double *correlation, double *pages);

#endif
