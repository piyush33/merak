package orders

import (
	"context"
	"database/sql"
)

type Repository interface {
	Get(ctx context.Context, id string) (*Order, error)
	Save(ctx context.Context, o *Order) error
}

type PostgresRepository struct {
	db *sql.DB
}

func NewPostgresRepository(db *sql.DB) *PostgresRepository {
	return &PostgresRepository{db: db}
}

func (r *PostgresRepository) Get(ctx context.Context, id string) (*Order, error) {
	row := r.db.QueryRowContext(ctx, `
		SELECT id, user_id, status, total
		FROM orders
		WHERE id = $1 AND deleted_at IS NULL`, id)
	var o Order
	if err := row.Scan(&o.ID, &o.UserID, &o.Status, &o.Total); err != nil {
		return nil, err
	}
	return &o, nil
}

func (r *PostgresRepository) Save(ctx context.Context, o *Order) error {
	_, err := r.db.ExecContext(ctx, "UPDATE orders SET status = $1, total = $2 WHERE id = $3", o.Status, o.Total, o.ID)
	return err
}
