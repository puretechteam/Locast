// apps/client/src/services/roomClient.ts
//
// Temporary file API calls for room lifecycle actions.
// P6-T06: temp file keep/delete operations invoked from LeaveRoomModal.

import { commands } from "./ipc";

export async function getTempFilesForRoom(roomId: string) {
    return await commands.getTempFiles(roomId);
}

export async function keepTempFiles(fileIds: string[]): Promise<void> {
    await commands.markFilesPermanent(fileIds);
}

export async function deleteTempFiles(fileIds: string[]): Promise<void> {
    await commands.deleteFilesToTrash(fileIds);
}
