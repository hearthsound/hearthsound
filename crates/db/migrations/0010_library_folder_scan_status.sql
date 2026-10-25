-- The outcome of a folder's last scan: a failure, or a completed scan that
-- refused to remove anything. NULL after a clean scan.
ALTER TABLE library_folders ADD COLUMN last_scan_error TEXT;
