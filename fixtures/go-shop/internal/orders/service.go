package orders

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"net/http"
)

var (
	ErrNotCancellable = errors.New("order cannot be cancelled")
	ErrForbidden      = errors.New("forbidden")
)

type Service struct {
	repo   Repository
	client *http.Client
	log    *slog.Logger
}

func NewService(repo Repository, client *http.Client, log *slog.Logger) *Service {
	return &Service{repo: repo, client: client, log: log}
}

func (s *Service) Cancel(ctx context.Context, userID, id string) error {
	o, err := s.repo.Get(ctx, id)
	if err != nil {
		return fmt.Errorf("get order: %w", err)
	}
	if o.UserID != userID {
		return ErrForbidden
	}
	if o.Status != StatusPending {
		return ErrNotCancellable
	}
	o.Status = StatusCancelled
	if err := s.repo.Save(ctx, o); err != nil {
		return err
	}
	s.log.Info("order cancelled", "id", id)
	s.refund(ctx, o)
	return nil
}

func (s *Service) Pay(ctx context.Context, id string) error {
	o, err := s.repo.Get(ctx, id)
	if err != nil {
		return err
	}
	if o.Status != StatusPending {
		return fmt.Errorf("order %s is %s", id, o.Status)
	}
	o.Status = StatusPaid
	return s.repo.Save(ctx, o)
}

func (s *Service) refund(ctx context.Context, o *Order) {
	body, _ := json.Marshal(map[string]any{"order": o.ID, "amount": o.Total})
	req, _ := http.NewRequestWithContext(ctx, http.MethodPost, "https://payments.example.com/refunds", bytes.NewReader(body))
	resp, err := s.client.Do(req)
	if err != nil {
		s.log.Error("refund failed", "err", err)
		return
	}
	resp.Body.Close()
}
