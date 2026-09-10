# Share files between computers

Open **Files** in Splice to show the file shelf. Both computers need a matching version of Splice and must be connected.

## Offer files

Choose the receiving computer, then drop files or folders into the shelf. Splice sends their names and sizes. The files stay on your computer, and their contents are not sent yet.

On the receiving computer, the offer appears in its shelf. Choose how to receive it:

- **Save to…** chooses a folder and starts the copy.
- **Receive to clipboard** downloads the selection and puts the received files on the local clipboard. Paste them into a file manager afterward.
- Drag the offer into a supported application. A fresh click and drag starts the receiving computer's native drag. The copy starts when the application requests the files after the drop.

Dropping into Splice and picking up on the other computer are separate gestures. You can cross the screen boundary normally between them.

## Copy and paste

Copy files in Finder, Dolphin or Files, then switch to the receiving computer. Open its file shelf and choose **Receive to clipboard**. When receiving finishes, paste into the destination folder.

If you copy something else while the files are arriving, Splice keeps your newer clipboard selection. The received files remain available in the shelf.

If a Linux clipboard portal session restarts or Splice switches clipboard backends, copy the selection again after Splice reconnects. Splice does not replay clipboard offers from the gap over a potentially newer local copy.

## Progress and cancellation

The shelf shows transfer progress and any error. Cancelling an unaccepted drag sends no file contents, and you can pick up the offer again. Cancelling an active transfer stops the copy.

Splice always copies. It never deletes the original files, including when the original clipboard selection used Cut. It does not overwrite an existing destination item silently.

An application may require local file contents before it accepts a drag. For these applications, receive the files first, then drag the received files. On Mac, receive folders before dragging them into another application.

Received files remain available after the shelf closes. File offers that have not been received expire after 24 hours without use, and the source computer must remain connected while receiving.

Use **Clear** on a received copy when you no longer need Splice to retain it. Files saved into your chosen folder stay there. Clearing a cached receipt invalidates its earlier Splice drag paths; finish pasting or reading those paths first. A receipt still in use by the clipboard or an active drag cannot be cleared.

## Files and limits

Files keep their ordinary permissions, executable bits, and supported modification timestamps. Relative symbolic links inside a selected folder remain links. Links that escape the selected folder, dangling links, special device files, and names that collide after case or Unicode normalization are rejected with an error. To share a top-level symbolic link, select its target instead.

Splice does not transfer ownership, ACLs, extended attributes, resource forks, or Finder tags. Use an archive when those attributes or complete application-bundle metadata matter.

One offer supports up to 128 selected roots and 4,096 entries, subject to a 256 KiB metadata limit and 64 directory levels. Larger selections need to be split or archived. The retained receipt cache has a 64 GiB limit; Clear frees cached receipts. Received data is not automatically evicted.
