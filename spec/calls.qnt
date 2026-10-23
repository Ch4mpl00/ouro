module calls {
  // Record states: absent, intent, success, response failure, proven not executed.
  pure def canDispatch(record: int, protected: bool): bool =
    record == 0 or record == 4 or (record == 1 and not(protected))
}
