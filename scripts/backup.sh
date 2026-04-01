#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────
# RICOZ SNIPER - Database Backup Script
# ─────────────────────────────────────────────────────────────
# Usage:
#   ./scripts/backup.sh                     # Local backup only
#   ./scripts/backup.sh --remote user@host  # Also upload via rsync
#
# Cron example (daily at 3 AM):
#   0 3 * * * /opt/ricoz-sniper/scripts/backup.sh >> /var/log/ricoz-sniper/backup.log 2>&1
# ─────────────────────────────────────────────────────────────
set -euo pipefail

# ── Configuration ───────────────────────────────────────────
APP_DIR="${APP_DIR:-/opt/ricoz-sniper}"
DB_FILE="${DB_FILE:-${APP_DIR}/data/ricoz-sniper.db}"
BACKUP_DIR="${BACKUP_DIR:-${APP_DIR}/backups}"
MAX_BACKUPS=7
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
BACKUP_FILE="${BACKUP_DIR}/ricoz-sniper_${TIMESTAMP}.db"

# ── Parse arguments ─────────────────────────────────────────
REMOTE_TARGET=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --remote)
            REMOTE_TARGET="$2"
            shift 2
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

# ── Validate ────────────────────────────────────────────────
if [ ! -f "${DB_FILE}" ]; then
    echo "[ERROR] Database file not found: ${DB_FILE}"
    exit 1
fi

mkdir -p "${BACKUP_DIR}"

# ── Create backup using SQLite online backup ────────────────
echo "[$(date)] Starting database backup..."

# Use sqlite3 .backup command for a consistent snapshot even
# if the database is being written to (WAL mode).
if command -v sqlite3 &>/dev/null; then
    sqlite3 "${DB_FILE}" ".backup '${BACKUP_FILE}'"
else
    # Fallback: simple file copy. Safe because WAL mode keeps
    # the main database file consistent.
    cp "${DB_FILE}" "${BACKUP_FILE}"
fi

# Compress the backup.
gzip "${BACKUP_FILE}"
BACKUP_FILE="${BACKUP_FILE}.gz"

BACKUP_SIZE=$(du -h "${BACKUP_FILE}" | cut -f1)
echo "[$(date)] Backup created: ${BACKUP_FILE} (${BACKUP_SIZE})"

# ── Rotate old backups (keep last MAX_BACKUPS) ──────────────
BACKUP_COUNT=$(find "${BACKUP_DIR}" -name "ricoz-sniper_*.db.gz" -type f | wc -l)
if [ "${BACKUP_COUNT}" -gt "${MAX_BACKUPS}" ]; then
    REMOVE_COUNT=$((BACKUP_COUNT - MAX_BACKUPS))
    echo "[$(date)] Removing ${REMOVE_COUNT} old backup(s)..."
    find "${BACKUP_DIR}" -name "ricoz-sniper_*.db.gz" -type f -printf '%T+ %p\n' \
        | sort \
        | head -n "${REMOVE_COUNT}" \
        | awk '{print $2}' \
        | xargs rm -f
fi

echo "[$(date)] Backups retained: $(find "${BACKUP_DIR}" -name "ricoz-sniper_*.db.gz" -type f | wc -l)/${MAX_BACKUPS}"

# ── Optional remote upload ──────────────────────────────────
if [ -n "${REMOTE_TARGET}" ]; then
    echo "[$(date)] Uploading backup to ${REMOTE_TARGET}..."
    rsync -az --progress "${BACKUP_FILE}" "${REMOTE_TARGET}:~/ricoz-backups/"
    echo "[$(date)] Remote upload complete."
fi

echo "[$(date)] Backup finished successfully."
